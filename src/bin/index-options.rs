use fc_search::{build_options_for_input, Flake};

fn main() {
    let fc_nixos = Flake {
        owner: "flyingcircusio".to_string(),
        name: "fc-nixos".to_string(),
        branch: "fc-23.11-dev".to_string(),
    };

    let vals = build_options_for_input(&fc_nixos).expect("the fc-23.11-dev branch failed to build");

    let outstring = serde_json::to_string(&vals).unwrap();
    std::fs::write("out.json", outstring).unwrap();
}
