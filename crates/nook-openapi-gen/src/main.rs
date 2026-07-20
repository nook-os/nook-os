use utoipa::OpenApi;

fn main() -> anyhow::Result<()> {
    println!(
        "{}",
        nook_control::openapi::ApiDoc::openapi().to_pretty_json()?
    );
    Ok(())
}
