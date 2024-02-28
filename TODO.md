- add the example field

- render strings to html with pandoc
    - enclose in a `<div hx-disable></div>`

- nix store path to github link
    - for both nixpkgs + fc-nixos

- improve tokenization and search of name field + search input

- datastore
    - reindex regularly (1/day at night?)
        - get newest evaluations for "fc-" jobs from hydra
        - try to generate options from those + reindex them
        - at runtime
