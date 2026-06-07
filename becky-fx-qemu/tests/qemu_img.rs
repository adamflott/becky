use becky_fx_qemu::img_json::QemuImgInfo;
use std::error::Error;
use std::process::Command;

#[test]
fn qemu_img_info_parses_real_raw_image_when_enabled() -> Result<(), Box<dyn Error>> {
    if std::env::var("BECKY_QEMU_INTEGRATION").as_deref() != Ok("1") {
        eprintln!("skipping qemu-img integration test; set BECKY_QEMU_INTEGRATION=1 to enable");
        return Ok(());
    }

    let qemu_img = match which::which("qemu-img") {
        Ok(path) => path,
        Err(err) => {
            eprintln!("skipping qemu-img integration test; qemu-img not found: {err}");
            return Ok(());
        }
    };

    let dir = std::env::temp_dir().join(format!("becky-qemu-img-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    let image = dir.join("disk.raw");

    let create = Command::new(&qemu_img)
        .args(["create", "--format", "raw"])
        .arg(&image)
        .arg("1048576")
        .output()?;
    assert!(create.status.success(), "qemu-img create failed: {}", String::from_utf8_lossy(&create.stderr));

    let info = Command::new(&qemu_img)
        .args(["info", "--force-share", "--output", "json"])
        .arg(&image)
        .output()?;
    assert!(info.status.success(), "qemu-img info failed: {}", String::from_utf8_lossy(&info.stderr));

    let parsed = serde_json::from_slice::<QemuImgInfo>(&info.stdout)?;
    assert_eq!(parsed.format, "raw");
    assert!(!parsed.is_corrupt());

    let _ = std::fs::remove_file(&image);
    let _ = std::fs::remove_dir(&dir);
    Ok(())
}
