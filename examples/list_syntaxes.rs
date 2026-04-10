fn main() {
    let ps = two_face::syntax::extra_newlines();
    println!("two-face extra-newlines pack: {} syntaxes", ps.syntaxes().len());
    for s in ps.syntaxes() {
        println!("  {} -> {:?}", s.name, s.file_extensions);
    }
}
