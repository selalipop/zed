fn main() {
    prost_build::compile_protos(&["src/unite.proto"], &["src/"]).unwrap();
}
