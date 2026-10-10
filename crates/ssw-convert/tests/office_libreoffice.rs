//! Opens the exported DOCX and ODT in a real LibreOffice, headless, and converts them to PDF and
//! plain text. A file LibreOffice cannot load, or one that loses the body text, fails the test.
//!
//! The test runs only when `soffice.exe` is available: set `SSW_SOFFICE` to its path, or install
//! LibreOffice at its default location. Otherwise it prints the reason and returns. LibreOffice
//! is started with `--headless`, without a console window and with a throw-away profile; when it
//! does not finish in time, the process tree this test started is killed and nothing else.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use ssw_convert::docx::{ExportOptions, MathImage, Resources, export_docx};
use ssw_convert::odt::export_odt;

/// Where the LibreOffice installer puts `soffice.exe`.
const DEFAULT_SOFFICE: &str = "C:/Program Files/LibreOffice/program/soffice.exe";
const TIMEOUT: Duration = Duration::from_secs(240);

const RICH: &str = "\
# Quartz Heading

Some **Amber** and *Basalt* and ~~Chert~~ text with a [link](https://example.org/a).

- Cobalt
    - Dolomite
- Epidote

1. Feldspar
2. Garnet

- [ ] Hematite
- [x] Iolite

> Jasper quote

```
kyanite = 1
```

---

Lepidolite[^1]

<!-- pagebreak -->

| Magnetite | Nephrite |
| :-- | --: |
| Obsidian | Peridot |

[^1]: Quartzite note.
";

/// Words LibreOffice's plain-text export must contain. That filter leaves footnote bodies out,
/// so these are checked separately in the flat ODT ([`footnote_text`]).
const WORDS: [&str; 16] = [
    "Quartz Heading",
    "Amber",
    "Basalt",
    "Chert",
    "Cobalt",
    "Dolomite",
    "Epidote",
    "Feldspar",
    "Garnet",
    "Hematite",
    "Iolite",
    "Jasper quote",
    "kyanite",
    "Lepidolite",
    "Magnetite",
    "Obsidian",
];

fn soffice() -> Option<PathBuf> {
    let path = std::env::var_os("SSW_SOFFICE")
        .map_or_else(|| PathBuf::from(DEFAULT_SOFFICE), PathBuf::from);
    path.is_file().then_some(path)
}

fn export(odt: bool) -> Vec<u8> {
    let mut image = |_: &str| None::<Vec<u8>>;
    let mut math = |_: &str, _: bool| -> Option<MathImage> { None };
    let mut resources = Resources {
        image: &mut image,
        math: &mut math,
    };
    let options = ExportOptions::default();
    if odt {
        export_odt(RICH, &options, &mut resources).unwrap()
    } else {
        export_docx(RICH, &options, &mut resources).unwrap()
    }
}

/// The text of the footnote bodies (`<text:note-body>`) of a flat ODT, tags removed.
fn footnote_text(fodt: &str) -> String {
    const OPEN: &str = "<text:note-body>";
    let mut text = String::new();
    let mut rest = fodt;
    while let Some(start) = rest.find(OPEN) {
        let body = &rest[start + OPEN.len()..];
        let end = body.find("</text:note-body>").unwrap_or(body.len());
        let mut in_tag = false;
        for c in body[..end].chars() {
            match c {
                '<' => in_tag = true,
                '>' => in_tag = false,
                _ if !in_tag => text.push(c),
                _ => {}
            }
        }
        text.push('\n');
        rest = &body[end..];
    }
    text
}

/// Stops the process tree rooted at `child` (only what this test started).
fn kill_tree(child: &mut std::process::Child) {
    let pid = child.id().to_string();
    let _ = Command::new("taskkill")
        .args(["/PID", pid.as_str(), "/T", "/F"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let _ = child.kill();
    let _ = child.wait();
}

/// Converts `input` with `filter` (`pdf`, `txt:Text`) into `out_dir`; returns the output file.
fn convert(
    soffice: &Path,
    profile: &Path,
    input: &Path,
    filter: &str,
    out_dir: &Path,
) -> Result<PathBuf, String> {
    std::fs::create_dir_all(out_dir).map_err(|e| e.to_string())?;
    let profile_url = format!(
        "-env:UserInstallation=file:///{}",
        profile
            .to_string_lossy()
            .replace('\\', "/")
            .replace(' ', "%20")
    );
    let mut command = Command::new(soffice);
    command
        .args([
            "--headless",
            "--norestore",
            "--nologo",
            profile_url.as_str(),
            "--convert-to",
            filter,
            "--outdir",
        ])
        .arg(out_dir)
        .arg(input)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = command
        .spawn()
        .map_err(|e| format!("cannot start {}: {e}", soffice.display()))?;
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() > TIMEOUT => {
                kill_tree(&mut child);
                return Err(format!(
                    "LibreOffice did not finish converting {} within {TIMEOUT:?}",
                    input.display()
                ));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(200)),
            Err(e) => return Err(e.to_string()),
        }
    };
    let extension = filter.split(':').next().unwrap_or(filter);
    let output = out_dir
        .join(input.file_stem().unwrap())
        .with_extension(extension);
    if status.success() && output.is_file() {
        Ok(output)
    } else {
        Err(format!(
            "LibreOffice exited with {status} and wrote {} = {}",
            output.display(),
            output.is_file()
        ))
    }
}

#[test]
fn exported_documents_open_in_libreoffice() {
    let Some(soffice) = soffice() else {
        eprintln!("skipped: no soffice.exe (set SSW_SOFFICE or install it at {DEFAULT_SOFFICE})");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let profile = dir.path().join("profile");
    for (extension, odt) in [("docx", false), ("odt", true)] {
        let input = dir.path().join(format!("rich.{extension}"));
        std::fs::write(&input, export(odt)).unwrap();
        let out = dir.path().join(format!("out-{extension}"));

        let pdf = convert(&soffice, &profile, &input, "pdf", &out)
            .unwrap_or_else(|e| panic!("{extension} to PDF: {e}"));
        let bytes = std::fs::read(&pdf).unwrap();
        assert!(
            bytes.starts_with(b"%PDF-") && bytes.len() > 1000,
            "{extension}: not a PDF of a real document ({} bytes)",
            bytes.len()
        );

        let txt = convert(&soffice, &profile, &input, "txt:Text", &out)
            .unwrap_or_else(|e| panic!("{extension} to text: {e}"));
        let text = String::from_utf8_lossy(&std::fs::read(&txt).unwrap()).to_lowercase();
        for word in WORDS {
            assert!(
                text.contains(&word.to_lowercase()),
                "{extension}: LibreOffice's text lacks {word:?}:\n{text}"
            );
        }

        // The footnote body: LibreOffice keeps it as a note in the flat ODT it writes.
        let fodt = convert(&soffice, &profile, &input, "fodt", &out)
            .unwrap_or_else(|e| panic!("{extension} to flat ODT: {e}"));
        let notes = footnote_text(&String::from_utf8_lossy(&std::fs::read(&fodt).unwrap()));
        assert!(
            notes.contains("Quartzite note"),
            "{extension}: LibreOffice lost the footnote body; its footnotes read {notes:?}"
        );
    }
}
