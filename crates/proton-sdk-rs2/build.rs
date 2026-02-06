fn main() {
    prost_build::compile_protos(&["protos/proton.sdk.proto"], &["protos"]).unwrap();
}