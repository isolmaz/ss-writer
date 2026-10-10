//! The built `SSWriter.exe` must carry its application manifest as the `RT_MANIFEST` resource;
//! without Common-Controls v6 the process fails to start (`TaskDialogIndirect` is not exported
//! by the v5 comctl32). The resource is read straight from the PE file: no code is loaded and
//! nothing is executed.

mod pe;

/// `RT_MANIFEST`.
const RT_MANIFEST: u32 = 24;

#[test]
fn the_executable_embeds_its_application_manifest_exactly_once() {
    let bytes = std::fs::read(env!("CARGO_BIN_EXE_SSWriter")).unwrap();
    let image = pe::Image::parse(&bytes).expect("SSWriter.exe has no resource directory");
    // The icon and version resources are linked next to the manifest: it must stay a single
    // RT_MANIFEST (a second one makes the linker fail or the loader pick the wrong one).
    let ids = image.resource_ids(RT_MANIFEST);
    assert_eq!(ids.len(), 1, "RT_MANIFEST ids: {ids:?}");
    let manifest = image
        .resource(RT_MANIFEST, ids[0])
        .expect("SSWriter.exe has no RT_MANIFEST resource");
    let text = String::from_utf8_lossy(manifest);
    for required in [
        // Common-Controls v6: TaskDialogIndirect lives only there.
        r#"name="Microsoft.Windows.Common-Controls""#,
        r#"version="6.0.0.0""#,
        r#"publicKeyToken="6595b64144ccf1df""#,
        // Windows 10 / 11 compatibility.
        "{8e0f7a12-bfb3-4fe8-b9a5-48fd50a15a9a}",
        // Per-monitor DPI v2 with the older fallback.
        "<dpiAwareness",
        "PerMonitorV2",
        ">true/pm</dpiAware>",
        // UTF-8 ANSI code page and long paths.
        "<activeCodePage",
        ">UTF-8</activeCodePage>",
        "<longPathAware",
        ">true</longPathAware>",
        // No elevation.
        r#"level="asInvoker""#,
    ] {
        assert!(
            text.contains(required),
            "manifest lacks {required}:\n{text}"
        );
    }
}

#[test]
fn the_resource_reader_rejects_files_that_are_not_pe_executables_with_resources() {
    assert!(pe::Image::parse(b"not an executable").is_none());
    let mut fake = vec![0u8; 0x200];
    fake[0x3C] = 0x80;
    assert!(pe::Image::parse(&fake).is_none(), "no PE signature");
    fake[0x80..0x84].copy_from_slice(b"PE\0\0");
    assert!(pe::Image::parse(&fake).is_none(), "not PE32+");
    // PE32+ magic, but an empty resource data directory.
    fake[0x80 + 24..0x80 + 26].copy_from_slice(&0x20B_u16.to_le_bytes());
    assert!(pe::Image::parse(&fake).is_none(), "no resource directory");
}
