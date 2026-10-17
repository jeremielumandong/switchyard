//! What a file is, from its name: the Type column, the row icon, and whether a double
//! click opens it here (text, code, images) or in the computer's default app (PDFs,
//! office documents, media, archives).

/// A file's family.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileKind {
    /// A folder.
    Folder,
    /// Plain text, logs, notes.
    Text,
    /// Source code, scripts, SQL.
    Code,
    /// JSON, YAML, TOML, XML, INI, env files.
    Config,
    /// Images the viewer shows.
    Image,
    /// PDF.
    Pdf,
    /// Word processor documents.
    Document,
    /// Spreadsheets.
    Spreadsheet,
    /// Slide decks.
    Presentation,
    /// Zip, tar and friends.
    Archive,
    /// Sound.
    Audio,
    /// Video.
    Video,
    /// Programs and installers.
    Program,
    /// Anything else: the editor sniffs it and shows a binary card if it is not text.
    Other,
}

impl FileKind {
    /// The family of a file named `name` (folders are known from the listing).
    pub fn of(name: &str) -> Self {
        let lower = name.to_ascii_lowercase();
        // Well-known extensionless names.
        match lower.as_str() {
            "makefile" | "dockerfile" | "justfile" | "rakefile" | "gemfile" | "procfile"
            | "vagrantfile" | "jenkinsfile" => return FileKind::Code,
            "readme" | "license" | "licence" | "changelog" | "authors" | "notice" | "copying"
            | "todo" => return FileKind::Text,
            ".env" | ".gitignore" | ".gitattributes" | ".editorconfig" | ".dockerignore"
            | ".npmrc" | ".bashrc" | ".zshrc" | ".profile" | ".bash_profile" | ".gitconfig" => {
                return FileKind::Config;
            }
            _ => {}
        }
        let Some((stem, ext)) = lower.rsplit_once('.') else {
            return FileKind::Other;
        };
        if stem.is_empty() {
            // `.env.local`-style dotfiles without a known name.
            return FileKind::Config;
        }
        match ext {
            "txt" | "md" | "markdown" | "rst" | "log" | "csv" | "tsv" | "adoc" | "org" | "tex"
            | "srt" | "vtt" | "diff" | "patch" => FileKind::Text,
            "rs" | "go" | "py" | "rb" | "js" | "mjs" | "cjs" | "ts" | "tsx" | "jsx" | "java"
            | "kt" | "kts" | "scala" | "c" | "h" | "cc" | "cpp" | "hpp" | "cs" | "fs" | "swift"
            | "m" | "mm" | "php" | "pl" | "lua" | "r" | "dart" | "ex" | "exs" | "erl" | "hs"
            | "clj" | "sql" | "sh" | "bash" | "zsh" | "fish" | "ps1" | "psm1" | "bat" | "cmd"
            | "html" | "htm" | "css" | "scss" | "sass" | "less" | "vue" | "svelte" | "astro"
            | "zig" | "nim" | "vim" | "el" | "tf" | "hcl" | "proto" | "graphql" | "gql"
            | "gradle" | "cmake" | "mk" | "nix" | "sol" | "wat" => FileKind::Code,
            "json" | "jsonc" | "json5" | "yaml" | "yml" | "toml" | "xml" | "ini" | "cfg"
            | "conf" | "config" | "env" | "properties" | "plist" | "lock" | "editorconfig"
            | "service" | "socket" | "timer" | "desktop" | "pem" | "crt" | "pub" | "csr"
            | "ndjson" | "jsonl" | "gitignore" => FileKind::Config,
            "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "ico" | "tif" | "tiff" | "svg" => {
                FileKind::Image
            }
            "pdf" => FileKind::Pdf,
            "doc" | "docx" | "odt" | "rtf" | "pages" | "epub" => FileKind::Document,
            "xls" | "xlsx" | "xlsm" | "ods" | "numbers" => FileKind::Spreadsheet,
            "ppt" | "pptx" | "odp" | "key" => FileKind::Presentation,
            "zip" | "tar" | "gz" | "tgz" | "bz2" | "xz" | "zst" | "7z" | "rar" | "jar" | "war"
            | "deb" | "rpm" | "iso" | "dmg" => FileKind::Archive,
            "mp3" | "wav" | "flac" | "aac" | "ogg" | "m4a" | "opus" | "aiff" => FileKind::Audio,
            "mp4" | "mov" | "mkv" | "avi" | "webm" | "wmv" | "m4v" | "mpg" | "mpeg" => {
                FileKind::Video
            }
            "exe" | "msi" | "app" | "pkg" | "appimage" | "apk" | "dll" | "so" | "dylib" => {
                FileKind::Program
            }
            // `.env.local` and other dotfiles.
            _ if lower.starts_with('.') => FileKind::Config,
            _ => FileKind::Other,
        }
    }

    /// Whether a double click opens it in Switchyard (editor or image viewer); the rest
    /// open in the computer's default app.
    pub fn opens_here(self) -> bool {
        matches!(
            self,
            FileKind::Text | FileKind::Code | FileKind::Config | FileKind::Image | FileKind::Other
        )
    }

    /// The Type column.
    pub fn label(self) -> &'static str {
        match self {
            FileKind::Folder => "Folder",
            FileKind::Text => "Text",
            FileKind::Code => "Source code",
            FileKind::Config => "Configuration",
            FileKind::Image => "Image",
            FileKind::Pdf => "PDF document",
            FileKind::Document => "Document",
            FileKind::Spreadsheet => "Spreadsheet",
            FileKind::Presentation => "Presentation",
            FileKind::Archive => "Archive",
            FileKind::Audio => "Audio",
            FileKind::Video => "Video",
            FileKind::Program => "Program",
            FileKind::Other => "File",
        }
    }

    /// The row icon (Lucide, from gpui-kit's assets).
    pub fn icon(self) -> &'static str {
        match self {
            FileKind::Folder => "icons/swy-folder.svg",
            FileKind::Text => "icons/swy-file-text.svg",
            FileKind::Code => "icons/swy-file-code.svg",
            FileKind::Config => "icons/swy-file-cog.svg",
            FileKind::Image => "icons/swy-file-image.svg",
            FileKind::Pdf => "icons/swy-file-type.svg",
            FileKind::Document => "icons/swy-file-pen.svg",
            FileKind::Spreadsheet => "icons/swy-file-spreadsheet.svg",
            FileKind::Presentation => "icons/swy-file-chart-pie.svg",
            FileKind::Archive => "icons/swy-file-archive.svg",
            FileKind::Audio => "icons/swy-file-music.svg",
            FileKind::Video => "icons/swy-file-video-camera.svg",
            FileKind::Program => "icons/swy-file-terminal.svg",
            FileKind::Other => "icons/swy-file.svg",
        }
    }
}

/// `report.pdf` → `PDF document`; with the extension for plain files (`BIN file`).
pub fn type_label(name: &str, kind: FileKind) -> String {
    match (kind, name.rsplit_once('.')) {
        (FileKind::Other, Some((stem, ext))) if !stem.is_empty() && ext.len() <= 8 => {
            format!("{} file", ext.to_ascii_uppercase())
        }
        _ => kind.label().to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_by_name() {
        assert_eq!(FileKind::of("main.RS"), FileKind::Code);
        assert_eq!(FileKind::of("Makefile"), FileKind::Code);
        assert_eq!(FileKind::of("notes.md"), FileKind::Text);
        assert_eq!(FileKind::of(".env"), FileKind::Config);
        assert_eq!(FileKind::of(".env.local"), FileKind::Config);
        assert_eq!(FileKind::of("Cargo.lock"), FileKind::Config);
        assert_eq!(FileKind::of("logo.PNG"), FileKind::Image);
        assert_eq!(FileKind::of("q3.xlsx"), FileKind::Spreadsheet);
        assert_eq!(FileKind::of("backup.tar.gz"), FileKind::Archive);
        assert_eq!(FileKind::of("data.bin"), FileKind::Other);
        assert_eq!(FileKind::of("LICENSE"), FileKind::Text);
    }

    #[test]
    fn common_files_open_here_and_the_rest_outside() {
        for name in ["a.rs", "a.json", "a.png", "a.log", "unknown", "a.bin"] {
            assert!(FileKind::of(name).opens_here(), "{name}");
        }
        for name in ["a.pdf", "a.docx", "a.mp4", "a.zip", "a.exe", "a.pptx"] {
            assert!(!FileKind::of(name).opens_here(), "{name}");
        }
    }

    #[test]
    fn type_column() {
        assert_eq!(type_label("x.pdf", FileKind::Pdf), "PDF document");
        assert_eq!(type_label("x.bin", FileKind::Other), "BIN file");
        assert_eq!(type_label("blob", FileKind::Other), "File");
    }
}
