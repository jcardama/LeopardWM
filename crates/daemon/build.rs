fn main() {
    let mut res = winresource::WindowsResource::new();
    res.set_icon("../../assets/leopardwm.ico");
    res.compile().expect("Failed to compile Windows resources");
}
