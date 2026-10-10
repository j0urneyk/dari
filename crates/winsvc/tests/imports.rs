#![cfg(windows)]

use object::LittleEndian as LE;
use object::read::pe::PeFile64;

fn imported_dlls(data: &[u8]) -> object::Result<Vec<String>> {
    let file = PeFile64::parse(data)?;
    let sections = file.section_table();
    let directories = file.data_directories();
    let lower = |name: &[u8]| String::from_utf8_lossy(name).to_lowercase();
    let mut dlls = Vec::new();
    if let Some(imports) = file.import_table()? {
        let mut descriptors = imports.descriptors()?;
        while let Some(descriptor) = descriptors.next()? {
            dlls.push(lower(imports.name(descriptor.name.get(LE))?));
        }
    }
    if let Some(delayed) = directories.delay_load_import_table(data, &sections)? {
        let mut descriptors = delayed.descriptors()?;
        while let Some(descriptor) = descriptors.next()? {
            dlls.push(lower(delayed.name(descriptor.dll_name_rva.get(LE))?));
        }
    }
    Ok(dlls)
}

const NETWORKING: [&str; 9] = [
    "ws2_32.dll",
    "wsock32.dll",
    "mswsock.dll",
    "winhttp.dll",
    "winhttpcom.dll",
    "wininet.dll",
    "websocket.dll",
    "dnsapi.dll",
    "iphlpapi.dll",
];

#[test]
fn dari_service_imports_no_networking() {
    let data = std::fs::read(env!("CARGO_BIN_EXE_dari-service")).unwrap();
    let dlls = imported_dlls(&data).unwrap();
    assert!(dlls.iter().any(|dll| dll == "kernel32.dll"), "{dlls:?}");
    let networking: Vec<&String> = dlls
        .iter()
        .filter(|dll| NETWORKING.contains(&dll.as_str()))
        .collect();
    assert!(networking.is_empty(), "imports {networking:?}");
}
