//! QR rendering regression test: the PNG fallback must produce a decodable
//! QR that round-trips to the encoded content. Guards the 2026-11-17 fix
//! where the ANSI renderer painted inverted colors on dark terminals and
//! phone scanners silently failed, leaving QR logins stuck on Waiting.

#[path = "../src/qr_terminal.rs"]
mod qr_terminal;

/// Decode the first QR grid found in a greyscale-loaded image file.
fn decode_png(path: &str) -> String {
    let img = image::open(path)
        .unwrap_or_else(|e| panic!("open {path}: {e}"))
        .to_luma8();
    let mut prepared = rqrr::PreparedImage::prepare_from_greyscale(
        img.width() as usize,
        img.height() as usize,
        |x, y| img.get_pixel(x as u32, y as u32).0[0],
    );
    let grids = prepared.detect_grids();
    let grid = grids
        .first()
        .unwrap_or_else(|| panic!("no QR grid detected in {path}"));
    let (_, content) = grid
        .decode()
        .unwrap_or_else(|e| panic!("decode {path}: {e}"));
    content
}

#[test]
fn png_roundtrip_recovers_the_encoded_url() {
    let url = "https://passport.bilibili.com/h5-app/passport/login?auth_code=roundtrip-test";
    let path = std::env::temp_dir().join("musid_dli_qr_roundtrip.png");
    let path = path.to_str().expect("tempdir is valid utf-8").to_owned();
    qr_terminal::render_png(&path, url).expect("render_png");
    assert_eq!(decode_png(&path), url);
    let _ = std::fs::remove_file(&path);
}
