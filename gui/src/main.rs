use fiber::Config;
use fnn_gui::ui::tui_render;

fn main() -> Result<(), String> {
    let config = Config::parse();
    tui_render(&config)?;
    Ok(())
}
