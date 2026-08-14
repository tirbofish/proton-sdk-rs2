fn main() {
    let mut config = prost_build::Config::new();
    config.extern_path(".google.protobuf.Timestamp", "crate::utils::Timestamp");
    config.extern_path(".google.protobuf.Any", "crate::utils::Any");
    config.type_attribute(".", "#[derive(serde::Serialize, serde::Deserialize)]");
    config
        .compile_protos(&["proton.sdk.proto"], &["."])
        .unwrap();
}
