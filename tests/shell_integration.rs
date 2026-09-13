use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

/// The temporary directory with any 8.3 short components resolved.
///
/// Some Windows hosts put a short path in `TEMP` (`C:\Users\RUNNER~1\...`)
/// while the shells under test report the long form of the same directory,
/// so paths built here would never match what they print back.
fn temp_dir() -> PathBuf {
    let temp = std::env::temp_dir();
    let Ok(canonical) = std::fs::canonicalize(&temp) else {
        return temp;
    };
    let text = canonical.to_string_lossy().into_owned();
    PathBuf::from(text.strip_prefix(r"\\?\").unwrap_or(&text))
}

struct Fixture {
    root: PathBuf,
    destination: PathBuf,
    exe: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = temp_dir().join(format!("tadoru-shell-{}-{nonce}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        let destination = root.join("日本語 space & ! % [dir]");
        std::fs::create_dir(&destination).unwrap();
        let exe = root.join(format!("tadoru{}", std::env::consts::EXE_SUFFIX));
        let output = Command::new("rustc")
            .args(["--edition=2024", "--crate-name", "fixture_command"])
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/command.rs"))
            .arg("-o")
            .arg(&exe)
            .output()
            .unwrap();
        check(output);
        std::fs::copy(
            &exe,
            root.join(format!("zoxide{}", std::env::consts::EXE_SUFFIX)),
        )
        .unwrap();
        Self {
            root,
            destination,
            exe,
        }
    }

    fn command(&self, shell: &str) -> Command {
        let mut command = Command::new(shell);
        let mut paths = vec![self.root.clone()];
        paths.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        command
            .current_dir(&self.root)
            .env("PATH", std::env::join_paths(paths).unwrap())
            .env("TEST_PATH", &self.destination)
            .env("TEST_QUERY_LOG", self.root.join("query.txt"))
            .env("TEST_EXIT", "0")
            .env("TADORU_QUERY", "previous value")
            .env_remove("TADORU_PREVIOUS")
            .env("TEST_ROOT", &self.root)
            .env("TADORU_CONFIG_DIR", self.root.join("config"))
            .env("TEST_QUERY", "日本語 ^ & | % ! space");
        command
    }

    fn init(&self, shell: &str, files: bool) -> String {
        let mut command = Command::new(env!("CARGO_BIN_EXE_tadoru"));
        command.args(["init", shell]);
        if files {
            command.arg("--out").arg(&self.root);
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // Only remove the uniquely created test directory directly below the temp root.
        assert_eq!(self.root.parent(), Some(temp_dir().as_path()));
        std::fs::remove_dir_all(&self.root).unwrap();
    }
}

fn check(output: Output) {
    assert!(
        output.status.success(),
        "status: {}\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn powershell_scripts_and_functions_preserve_shell_state() {
    let fixture = Fixture::new();
    fixture.init("powershell", true);
    let functions = fixture.init("powershell", false).replace(
        &env!("CARGO_BIN_EXE_tadoru").replace('\'', "''"),
        &fixture.exe.to_string_lossy().replace('\'', "''"),
    );
    let script = format!(
        r#"
$ErrorActionPreference = 'Stop'
{functions}
$encoding = [Console]::OutputEncoding
foreach ($style in 'script', 'function') {{
    Set-Location -LiteralPath $env:TEST_ROOT
    if ($style -eq 'script') {{ & (Join-Path $env:TEST_ROOT 'z.ps1') }} else {{ z }}
    if ($LASTEXITCODE -ne 0 -or (Get-Location).Path -ne $HOME) {{ throw 'z without arguments did not go home' }}
    foreach ($name in 'c', 'cf', 'zi', 'z') {{
        foreach ($code in 0, 1, 2) {{
            Set-Location -LiteralPath $env:TEST_ROOT
            $env:TEST_EXIT = [string]$code
            $previous = $env:TADORU_PREVIOUS
            if ($style -eq 'script') {{ & (Join-Path $env:TEST_ROOT "$name.ps1") $env:TEST_QUERY }}
            else {{ & $name $env:TEST_QUERY }}
            if ($LASTEXITCODE -ne $code) {{ throw "wrong status: $style $name $code => $LASTEXITCODE" }}
            $expected = if ($code -eq 0) {{ $env:TEST_PATH }} else {{ $env:TEST_ROOT }}
            if ((Get-Location).Path -ne $expected) {{ throw "wrong cwd: $style $name $code" }}
            if ($code -eq 0) {{
                if ($env:TADORU_PREVIOUS -ne $env:TEST_ROOT) {{ throw 'previous directory not saved' }}
                if ($style -eq 'script') {{ & (Join-Path $env:TEST_ROOT 'c.ps1') '-' }} else {{ c '-' }}
                if ($LASTEXITCODE -ne 0 -or (Get-Location).Path -ne $env:TEST_ROOT) {{ throw 'back failed' }}
                if ($style -eq 'script') {{ & (Join-Path $env:TEST_ROOT 'c.ps1') '-' }} else {{ c '-' }}
                if ($LASTEXITCODE -ne 0 -or (Get-Location).Path -ne $env:TEST_PATH) {{ throw 'round trip failed' }}
            }} elseif ($env:TADORU_PREVIOUS -ne $previous) {{ throw 'failed pick changed previous directory' }}
            if ($env:TADORU_QUERY -ne 'previous value') {{ throw 'query was not restored' }}
            if ([Console]::OutputEncoding.CodePage -ne $encoding.CodePage) {{ throw 'encoding was not restored' }}
            if ([IO.File]::ReadAllText($env:TEST_QUERY_LOG) -ne $env:TEST_QUERY) {{ throw 'query changed' }}
        }}
    }}
}}
"#
    );
    let script_path = fixture.root.join("check.ps1");
    std::fs::write(&script_path, script).unwrap();
    check(
        fixture
            .command("pwsh")
            .args(["-NoProfile", "-NonInteractive", "-File"])
            .arg(script_path)
            .output()
            .unwrap(),
    );
}

#[cfg(windows)]
#[test]
fn cmd_scripts_preserve_status_codepage_and_directory() {
    let fixture = Fixture::new();
    fixture.init("cmd", true);
    let script = r#"@echo off
setlocal DisableDelayedExpansion
chcp 65001 >nul
for /f "tokens=2 delims=:" %%c in ('chcp') do set "BEFORE_CP=%%c"
call c.cmd test-query
set "ACTUAL_EXIT=%ERRORLEVEL%"
if not "%ACTUAL_EXIT%"=="%TEST_EXIT%" exit /b 10
if "%TEST_EXIT%"=="0" (if not "%CD%"=="%TEST_PATH%" exit /b 11) else (if not "%CD%"=="%TEST_ROOT%" exit /b 12)
if not "%TADORU_QUERY%"=="previous value" exit /b 13
for /f "tokens=2 delims=:" %%c in ('chcp') do if not "%%c"=="%BEFORE_CP%" exit /b 14
if not "%TEST_EXIT%"=="0" exit /b 0
if not "%TADORU_PREVIOUS%"=="%TEST_ROOT%" exit /b 15
call c.cmd -
if errorlevel 1 exit /b 16
if not "%CD%"=="%TEST_ROOT%" exit /b 17
call c.cmd -
if errorlevel 1 exit /b 18
if not "%CD%"=="%TEST_PATH%" exit /b 19
exit /b 0
"#;
    std::fs::write(fixture.root.join("check.cmd"), script.replace('\n', "\r\n")).unwrap();
    for name in ["c", "cf", "zi", "z"] {
        std::fs::write(
            fixture.root.join("check.cmd"),
            script
                .replace(
                    "call c.cmd test-query",
                    &format!("call {name}.cmd test-query"),
                )
                .replace('\n', "\r\n"),
        )
        .unwrap();
        for code in ["0", "1", "2"] {
            check(
                fixture
                    .command("cmd")
                    .args(["/d", "/c", "check.cmd"])
                    .env("TEST_EXIT", code)
                    .output()
                    .unwrap(),
            );
            assert_eq!(
                std::fs::read_to_string(fixture.root.join("query.txt")).unwrap(),
                "test-query"
            );
        }
    }
    // A legacy code page must be restored after reading the UTF-8 destination.
    std::fs::write(
        fixture.root.join("check.cmd"),
        script
            .replace("chcp 65001", "chcp 932")
            .replace('\n', "\r\n"),
    )
    .unwrap();
    check(
        fixture
            .command("cmd")
            .args(["/d", "/c", "check.cmd"])
            .output()
            .unwrap(),
    );
    std::fs::write(fixture.root.join("home.cmd"), "@echo off\r\ncall z.cmd\r\nif errorlevel 1 exit /b 10\r\nif not \"%CD%\"==\"%USERPROFILE%\" exit /b 11\r\nexit /b 0\r\n").unwrap();
    check(
        fixture
            .command("cmd")
            .env("USERPROFILE", &fixture.destination)
            .args(["/d", "/c", "home.cmd"])
            .output()
            .unwrap(),
    );
}

#[test]
fn bash_functions_preserve_shell_state() {
    let fixture = Fixture::new();
    let functions = fixture.init("bash", false).replace(
        &env!("CARGO_BIN_EXE_tadoru").replace('\\', "/"),
        &fixture.exe.to_string_lossy().replace('\\', "/"),
    );
    let script = format!(
        r#"
{functions}
HOME=$TEST_ROOT
z || exit 15
[ "$PWD" = "$(cd -- "$TEST_ROOT" && pwd)" ] || exit 16
for name in c cf zi z; do
    for code in 0 1 2; do
        cd -- "$TEST_ROOT" || exit 10
        export TEST_EXIT=$code
        previous=${{TADORU_PREVIOUS-}}
        "$name" "$TEST_QUERY"
        status=$?
        [ "$status" = "$code" ] || exit 11
        expected=$TEST_ROOT
        [ "$code" = 0 ] && expected=$TEST_PATH
        expected=$(cd -- "$expected" && pwd)
        [ "$PWD" = "$expected" ] || exit 12
        if [ "$code" = 0 ]; then
            c - || exit 17
            [ "$PWD" = "$(cd -- "$TEST_ROOT" && pwd)" ] || exit 18
            c - || exit 19
            [ "$PWD" = "$expected" ] || exit 20
        else
            [ "${{TADORU_PREVIOUS-}}" = "$previous" ] || exit 21
        fi
        [ "$TADORU_QUERY" = 'previous value' ] || exit 13
        [ "$(cat "$TEST_QUERY_LOG")" = "$TEST_QUERY" ] || exit 14
    done
done
"#
    );
    run_bash(&fixture, &script);
}

/// Runs `script` in a bash with no startup files, giving it the fixture's
/// paths in the forward-slash form bash on Windows expects.
fn run_bash(fixture: &Fixture, script: &str) {
    #[cfg(windows)]
    let shell = std::env::var("TADORU_TEST_BASH")
        .unwrap_or_else(|_| "C:/Program Files/Git/bin/bash.exe".into());
    #[cfg(not(windows))]
    let shell = "bash".to_string();
    check(
        fixture
            .command(&shell)
            .env(
                "TEST_PATH",
                fixture.destination.to_string_lossy().replace('\\', "/"),
            )
            .env(
                "TEST_ROOT",
                fixture.root.to_string_lossy().replace('\\', "/"),
            )
            .env(
                "TEST_QUERY_LOG",
                fixture
                    .root
                    .join("query.txt")
                    .to_string_lossy()
                    .replace('\\', "/"),
            )
            .args(["--noprofile", "--norc", "-c", script])
            .output()
            .unwrap(),
    );
}

#[test]
fn bash_defines_the_names_it_was_given_and_nothing_else() {
    let fixture = Fixture::new();
    let output = Command::new(env!("CARGO_BIN_EXE_tadoru"))
        .args(["init", "bash", "--cmd", "j", "--no-z"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let functions = String::from_utf8(output.stdout).unwrap().replace(
        &env!("CARGO_BIN_EXE_tadoru").replace('\\', "/"),
        &fixture.exe.to_string_lossy().replace('\\', "/"),
    );
    // The renamed pickers answer to the new names and keep the - shortcut,
    // while c, z and zi are not defined at all.
    let script = format!(
        r#"
{functions}
declare -F c >/dev/null && exit 30
declare -F z >/dev/null && exit 31
declare -F zi >/dev/null && exit 32
cd -- "$TEST_ROOT" || exit 33
export TEST_EXIT=0
j "$TEST_QUERY" || exit 34
[ "$PWD" = "$(cd -- "$TEST_PATH" && pwd)" ] || exit 35
j - || exit 36
[ "$PWD" = "$(cd -- "$TEST_ROOT" && pwd)" ] || exit 37
jf "$TEST_QUERY" || exit 38
[ "$PWD" = "$(cd -- "$TEST_PATH" && pwd)" ] || exit 39
"#
    );
    run_bash(&fixture, &script);
}

#[test]
fn recent_select_one_distinguishes_errors_empty_history_and_selection() {
    let fixture = Fixture::new();
    let mut command = fixture.command(env!("CARGO_BIN_EXE_tadoru"));
    command
        .args(["pick", "--mode", "recent", "--select-1"])
        .env_remove("TADORU_QUERY")
        .env("APPDATA", &fixture.root)
        .env("XDG_CONFIG_HOME", &fixture.root);
    let output = command.env("TEST_EXIT", "2").output().unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("fixture error 2"));
    assert!(output.stdout.is_empty());
    let output = command
        .env("TEST_EXIT", "0")
        .env_remove("TEST_PATH")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let output = command
        .env("TEST_PATH", &fixture.destination)
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().trim(),
        fixture.destination.to_string_lossy()
    );
    let zoxide = fixture
        .root
        .join(format!("zoxide{}", std::env::consts::EXE_SUFFIX));
    std::fs::remove_file(zoxide).unwrap();
    let output = command.env("PATH", &fixture.root).output().unwrap();
    assert_eq!(output.status.code(), Some(2));
    // Naming the missing program is not enough on its own: the reader also has
    // to learn how to get the mode working and what still works without it.
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("zoxide is not installed"), "{stderr}");
    assert!(stderr.contains("winget install"), "{stderr}");
    assert!(stderr.contains("favorites"), "{stderr}");
}

#[test]
fn favorites_cli_and_picker_share_persistent_storage_without_zoxide() {
    let fixture = Fixture::new();
    let command = || {
        let mut command = fixture.command(env!("CARGO_BIN_EXE_tadoru"));
        command
            .env_remove("TADORU_QUERY")
            .env("APPDATA", &fixture.root)
            .env("XDG_CONFIG_HOME", &fixture.root)
            .env("HOME", &fixture.root);
        command
    };
    check(
        command()
            .args(["favorite", "add"])
            .arg(&fixture.destination)
            .output()
            .unwrap(),
    );
    check(
        command()
            .args(["favorite", "add"])
            .arg(&fixture.destination)
            .output()
            .unwrap(),
    );
    let output = command().args(["favorite", "list"]).output().unwrap();
    assert!(output.status.success());
    let listed = String::from_utf8(output.stdout).unwrap();
    assert_eq!(listed.lines().count(), 1);
    let output = command()
        .args(["pick", "--mode", "favorites", "--select-1"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(String::from_utf8(output.stdout).unwrap(), listed);
    std::fs::remove_dir(&fixture.destination).unwrap();
    check(
        command()
            .args(["favorite", "remove"])
            .arg(&fixture.destination)
            .output()
            .unwrap(),
    );
    let output = command()
        .args(["pick", "--mode", "favorites", "--select-1"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
}

#[test]
fn action_config_can_be_created_and_validated_without_overwriting_customizations() {
    let fixture = Fixture::new();
    let mut command = fixture.command(env!("CARGO_BIN_EXE_tadoru"));
    command
        .env("APPDATA", &fixture.root)
        .env("XDG_CONFIG_HOME", &fixture.root)
        .env("HOME", &fixture.root);
    let output = command.args(["actions", "init"]).output().unwrap();
    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    let file = PathBuf::from(String::from_utf8(output.stderr).unwrap().trim());
    assert!(
        file.starts_with(fixture.root.join("config")),
        "test config escaped its isolated directory: {}",
        file.display()
    );
    let original = std::fs::read(&file).unwrap();
    let output = command.output().unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(std::fs::read(&file).unwrap(), original);
    let check_config = || {
        let mut command = fixture.command(env!("CARGO_BIN_EXE_tadoru"));
        command
            .env("APPDATA", &fixture.root)
            .env("XDG_CONFIG_HOME", &fixture.root)
            .env("HOME", &fixture.root)
            .args(["actions", "check"]);
        command.output().unwrap()
    };
    check(check_config());
    std::fs::write(&file, r#"{"version":42,"actions":[]}"#).unwrap();
    let output = check_config();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("unsupported actions version"));
}
