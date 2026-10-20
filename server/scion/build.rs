//! macOS：把 Info.plist 嵌入 scion 的 `__TEXT,__info_plist` 段。
//!
//! 签了 Developer ID 的命令行工具按自己的身份接受本地网络隐私检查；没有
//! `NSLocalNetworkUsageDescription` 时系统不弹授权框，直接拒绝向局域网发起的连接
//! （其他节点、缓存上游、局域网里的数据库和对象存储）。CFBundleIdentifier 与
//! scripts/sign-macos.sh 的 identifier 一致。

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS")? != "macos" {
        return Ok(());
    }
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleIdentifier</key>
	<string>com.opencsg.scion</string>
	<key>CFBundleName</key>
	<string>scion</string>
	<key>CFBundleInfoDictionaryVersion</key>
	<string>6.0</string>
	<key>CFBundleShortVersionString</key>
	<string>{version}</string>
	<key>NSLocalNetworkUsageDescription</key>
	<string>scion connects to other Scion nodes, databases and object storage on your local network.</string>
</dict>
</plist>
"#,
        version = std::env::var("CARGO_PKG_VERSION")?
    );
    let path = std::path::PathBuf::from(std::env::var("OUT_DIR")?).join("Info.plist");
    std::fs::write(&path, plist)?;
    println!(
        "cargo:rustc-link-arg-bins=-Wl,-sectcreate,__TEXT,__info_plist,{}",
        path.display()
    );
    Ok(())
}
