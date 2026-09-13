//! Display-only Nerd Font glyphs; never add these to paths or matcher input.
//! Codepoints: https://github.com/ryanoasis/nerd-fonts/blob/master/glyphnames.json

use std::path::Path;

use ratatui::style::{Color, Style};
use ratatui::text::Span;

/// Colour only the icon, inheriting the row's background and modifiers.
pub fn span(prefix: &'static str) -> Span<'static> {
    let color = match prefix {
        "★ " => Color::Indexed(220),
        "\u{f07b} " | OPEN_FOLDER => Color::Indexed(75),
        "\u{f1c3} " => Color::Indexed(71),
        "\u{f1c2} " => Color::Indexed(75),
        "\u{f1c4} " => Color::Indexed(208),
        "\u{f1c1} " => Color::Indexed(203),
        "\u{f013} " => Color::Indexed(109),
        "\u{f1c0} " => Color::Indexed(179),
        "\u{f085} " => Color::Indexed(114),
        "\u{f031} " => Color::Indexed(180),
        "\u{f121} " => Color::Indexed(110),
        "\u{f1c5} " => Color::Indexed(176),
        "\u{f1c6} " => Color::Indexed(179),
        "\u{f1c7} " => Color::Indexed(80),
        "\u{f1c8} " => Color::Indexed(173),
        "\u{f15c} " => Color::Indexed(114),
        _ => return Span::raw(prefix),
    };
    Span::styled(prefix, Style::new().fg(color))
}

/// The directory the listing is inside, as opposed to one merely named in it.
/// Only the side columns can show it: in the middle column nothing is open.
pub const OPEN_FOLDER: &str = "\u{f07c} ";

pub fn prefix(name: &str, is_dir: bool, enabled: bool) -> &'static str {
    if !enabled {
        return "";
    }
    if is_dir {
        return "\u{f07b} ";
    }
    let extension = Path::new(name)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    match extension.as_str() {
        "xls" | "xlsx" | "xlsm" | "xlsb" | "xlt" | "xltx" | "xltm" | "ods" | "csv" | "tsv" => {
            "\u{f1c3} "
        }
        "doc" | "docx" | "docm" | "dot" | "dotx" | "dotm" | "odt" | "rtf" => "\u{f1c2} ",
        "ppt" | "pptx" | "pptm" | "pot" | "potx" | "potm" | "pps" | "ppsx" | "ppsm" | "odp" => {
            "\u{f1c4} "
        }
        "pdf" => "\u{f1c1} ",
        "ini" | "cfg" | "conf" | "toml" | "yaml" | "yml" | "json" | "jsonc" | "json5" | "xml" => {
            "\u{f013} "
        }
        "db" | "sqlite" | "sqlite3" | "sql" | "duckdb" => "\u{f1c0} ",
        "exe" | "msi" | "msix" | "com" => "\u{f085} ",
        "ttf" | "otf" | "woff" | "woff2" => "\u{f031} ",
        "rs" | "py" | "js" | "jsx" | "ts" | "tsx" | "c" | "h" | "cpp" | "hpp" | "go" | "java"
        | "rb" | "sh" | "ps1" | "html" | "css" => "\u{f121} ",
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "svg" | "ico" | "bmp" => "\u{f1c5} ",
        "zip" | "gz" | "tar" | "7z" | "rar" | "xz" | "bz2" => "\u{f1c6} ",
        "mp3" | "wav" | "flac" | "ogg" | "m4a" => "\u{f1c7} ",
        "mp4" | "mkv" | "webm" | "mov" | "avi" => "\u{f1c8} ",
        "md" | "txt" | "log" | "rst" => "\u{f15c} ",
        _ => "\u{f15b} ",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn office_documents_use_distinct_icons_with_case_insensitive_extensions() {
        let sheet = prefix("特性要因図.xlsx", false, true);
        assert_eq!(prefix("jitsurei_6.XLSM", false, true), sheet);
        assert_eq!(prefix("test.csv", false, true), sheet);
        let word = prefix("report.docx", false, true);
        let slides = prefix("slides.pptx", false, true);
        let pdf = prefix("report.pdf", false, true);
        let generic = prefix("unknown", false, true);
        let icons = [sheet, word, slides, pdf, generic];
        for (i, icon) in icons.iter().enumerate() {
            assert!(!icons[i + 1..].contains(icon));
        }
        assert_eq!(
            prefix("folder.xlsx", true, true),
            prefix("folder", true, true)
        );
        assert_eq!(prefix("report.xlsx", false, false), "");
    }
}
