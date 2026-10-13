mod actions;
mod blame;
mod diff;
mod format;
mod git;
mod history;
mod rpc;
mod server;
mod share;
mod text;
mod urlhandler;
mod view;

const USAGE: &str = "\
revs — line history for Zed

  revs lsp                          language server (started by the Zed extension)
  revs previous  <file> <row>       open the change before this line's current version
  revs next      <file> <row>       in a Revs diff, open the line's next change
  revs open-file <file> <row>       in a Revs diff, open the working file there
  revs url <revs://...>          handle a link from the line blame hover card
";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if !matches!(args.first().map(String::as_str), Some("--version" | "version")) {
        view::make_private_dirs();
    }
    let code = match args.first().map(String::as_str) {
        Some("lsp" | "--stdio") => {
            server::run();
            0
        }
        Some(command @ ("previous" | "changes" | "next" | "open-file")) => {
            actions::command_line(command, &args[1..])
        }
        Some("url") => match args.get(1).map(|url| actions::handle_url(url)) {
            Some(Ok(())) => 0,
            Some(Err(error)) => {
                eprintln!("revs: {error}");
                1
            }
            None => 2,
        },
        Some("--version" | "version") => {
            println!("revs {}", env!("CARGO_PKG_VERSION"));
            0
        }
        Some(flag) if flag.starts_with("-psn") => urlhandler::run(),
        // LaunchServices starts the app bundle without arguments to deliver a link.
        None if unsafe { libc::isatty(0) } == 0 => urlhandler::run(),
        _ => {
            eprint!("{USAGE}");
            2
        }
    };
    std::process::exit(code);
}
