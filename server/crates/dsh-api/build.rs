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

    // 构建元数据（部署版本标记）：DEFING_GIT_COMMIT 构建参数（docker --build-arg）> git 命令 > unknown。
    // 暴露于 /healthz、/readyz 与 Admin UI 页脚，便于确认部署产物是否为最新构建。
    let commit = std::env::var("DEFING_GIT_COMMIT")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            std::process::Command::new("git")
                .args(["rev-parse", "--short", "HEAD"])
                .current_dir(&manifest)
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        })
        .unwrap_or_else(|| "unknown".into());
    let build_time = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    println!("cargo:rustc-env=DEFING_GIT_COMMIT={commit}");
    println!("cargo:rustc-env=DEFING_BUILD_TIME={build_time}");

    tonic_prost_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_protos(&[proto_file], &[proto_dir])?;
    Ok(())
}
