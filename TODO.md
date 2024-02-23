- fc-search
    - nix store path to github link
        - for both nixpkgs + fc-nixos

- webui
    - htmx incremental search
    - backend via templates (askama)

- datastore
    - tantivy
        - try to index all channels
        - generate overview buttons for channel selection from successfully indexed channels
    - reindex regularly (1/day at night?)
        - get newest evaluations for "fc-" jobs from hydra
        - try to generate options from those
        - index them
