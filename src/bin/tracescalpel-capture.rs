fn main() -> std::io::Result<()> {
    tracescalpel::capture_preflight()?;
    tracescalpel::serve()
}
