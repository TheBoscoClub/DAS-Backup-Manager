fn main() {
    // Track scripts embedded via include_str! so cargo rebuilds when they change
    println!("cargo:rerun-if-changed=../scripts/backup-run.sh");
    println!("cargo:rerun-if-changed=../scripts/backup-verify.sh");
    println!("cargo:rerun-if-changed=../scripts/boot-archive-cleanup.sh");
}
