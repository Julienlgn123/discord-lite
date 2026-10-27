fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        // Ressource n°1 : icône de l'exe, reprise par la fenêtre.
        res.set_icon_with_id("assets/icon.ico", "1");
        res.set("FileDescription", "Discord-Lite");
        res.set("ProductName", "Discord-Lite");
        res.compile().expect("compilation des ressources Windows");
    }
}
