//! tonic-prost-build：从 proto/config.v1.proto 生成数据面 gRPC 代码（模块 05）。
//! 依赖系统 protoc（本机 3.19.6 可用）。

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let proto_dir = manifest.join("../../../proto");
    let proto_file = proto_dir.join("config.v1.proto");
    println!("cargo:rerun-if-changed={}", proto_file.display());
    // rust_embed proc macro 在编译期内嵌 admin/ 资源，但 cargo 指纹不追踪资源文件：
    // 不声明 rerun-if-changed 会导致 app.js/index.html 改动不触发 dsh-api 重编译，
    // release 二进制静默保留旧 UI（曾引发「更新后仍显示旧界面」）。
    let admin_dir = manifest.join("admin");
    println!("cargo:rerun-if-changed={}", admin_dir.display());
    tonic_prost_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_protos(&[proto_file], &[proto_dir])?;
    Ok(())
}
