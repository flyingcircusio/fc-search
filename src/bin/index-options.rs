use fc_search::{build_options_for_input, Flake};

fn main() {
    let fc_nixos = Flake {
        owner: "PhilTaken".to_string(),
        name: "fc-nixos".to_string(),
        branch: "flake2.0".to_string(),
    };

    let vals = build_options_for_input(&fc_nixos).expect("flake2.0 branch can be built");

    let outstring = serde_json::to_string(&vals).unwrap();
    std::fs::write("out.json", outstring).unwrap();
}
