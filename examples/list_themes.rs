use syntect::highlighting::ThemeSet;

fn main() {
    let ts = ThemeSet::load_defaults();
    let mut names: Vec<&String> = ts.themes.keys().collect();
    names.sort();
    for n in names {
        println!("{n}");
    }
}
