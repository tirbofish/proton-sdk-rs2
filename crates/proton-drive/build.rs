fn main() {
    let mut config = prost_build::Config::new();
    config.extern_path(
        ".google.protobuf.Timestamp",
        "::proton_sdk_rs2::utils::Timestamp",
    );
    config.extern_path(".google.protobuf.Any", "::proton_sdk_rs2::utils::Any");
    config.extern_path(".proton.sdk", "::proton_sdk_rs2::protobuf");
    config.type_attribute(".", "#[derive(serde::Serialize, serde::Deserialize)]");
    config
        .compile_protos(&["../../protos/proton.drive.sdk.proto"], &["../../protos"])
        .unwrap();
}
