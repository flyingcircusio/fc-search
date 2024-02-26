use fc_search::build_options_for_input;

fn main() {
    let fc_nixos_url = "github:PhilTaken/fc-nixos/flake2.0";

    let vals = build_options_for_input(fc_nixos_url).expect("flake2.0 branch can be built");

    let outstring = serde_json::to_string(&vals).unwrap();
    std::fs::write("out.json", outstring).unwrap();
}
