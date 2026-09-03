use denia_browser::cdp::CdpHandle;
#[tokio::main]
async fn main() {
    let url = std::env::args().nth(1).expect("ws url");
    match CdpHandle::connect(&url).await {
        Ok(_) => println!("CONNECT OK"),
        Err(e) => println!("CONNECT FAIL: {e}"),
    }
}