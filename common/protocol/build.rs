//! 用纯 Rust 的 protox 编译 proto，不依赖系统 protoc。

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../proto");
    let files = [
        "scion/v1/common.proto",
        "scion/v1/server_info.proto",
        "scion/v1/object.proto",
        "scion/v1/write.proto",
        "scion/v1/land.proto",
        "scion/v1/ref.proto",
        "scion/v1/query.proto",
        "scion/v1/auth.proto",
        "scion/v1/admin.proto",
        "scion/v1/repo.proto",
        "scion/v1/event.proto",
        "scion/v1/org.proto",
    ];
    let fds = protox::compile(files, [&root])?;
    // 全部 RPC 的路径（`/scion.v1.ObjectService/GetObjects`），供指标只给已知方法建时间序列
    let mut methods = Vec::new();
    for f in &fds.file {
        let package = f.package.clone().unwrap_or_default();
        for s in &f.service {
            for m in &s.method {
                methods.push(format!(
                    "/{package}.{}/{}",
                    s.name.clone().unwrap_or_default(),
                    m.name.clone().unwrap_or_default()
                ));
            }
        }
    }
    methods.sort();
    let list: String = methods.iter().map(|m| format!("    {m:?},\n")).collect();
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR")?).join("methods.rs");
    std::fs::write(out, format!("&[\n{list}]\n"))?;
    tonic_prost_build::configure().bytes(".").compile_fds(fds)?;
    println!("cargo:rerun-if-changed={}", root.display());
    Ok(())
}
