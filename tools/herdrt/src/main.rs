fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("usage: herdrt <tool> [args...]");
        eprintln!("tools: access-probe, progress-view, role-bind, activate-roles, package, update");
        eprintln!("       render-reference stays on the Python tools until the ledger cutover.");
        std::process::exit(2);
    }
    let (cmd, rest) = (&args[0], &args[1..]);
    let code = match cmd.as_str() {
        "access-probe" => herdrt::access_probe::cli(rest),
        "progress-view" => herdrt::progress_view::cli(rest),
        "role-bind" => herdrt::role_bind::cli(rest),
        "activate-roles" => herdrt::activate_roles::cli(rest),
        "package" => herdrt::package::cli(rest),
        "update" => herdrt::update_tool::cli(rest),
        "render-reference" => {
            // Byte-parity with herdr_ledger.py's parser lives on the Python side.
            eprintln!("render-reference must be run via Python until the ledger cutover:");
            eprintln!("  python3 tools/render_reference.py");
            2
        }
        _ => {
            eprintln!("ERROR: unknown tool '{cmd}'");
            2
        }
    };
    std::process::exit(code);
}
