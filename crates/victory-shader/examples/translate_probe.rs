//! Replay translation of one locally captured title shader container.
//! Usage: cargo run -p victory-shader --features host --example translate_probe -- FILE.dxbc

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args().nth(1).ok_or("usage: translate_probe FILE.dxbc")?;
    let bytes = std::fs::read(path)?;
    let shader = victory_shader::emit::translate(&bytes)?;
    println!("translated {} DXIL bytes", shader.dxil.len());
    match victory_shader::emit::validator::Validator::load(
        victory_shader::emit::validator::Validator::default_path().as_deref(),
    ) {
        Ok(mut validator) => {
            validator.validate(&mut shader.dxil.clone()).map_err(std::io::Error::other)?;
            println!("DXIL validated");
        }
        Err(error) => println!("DXIL validation unavailable: {error}"),
    }
    Ok(())
}
