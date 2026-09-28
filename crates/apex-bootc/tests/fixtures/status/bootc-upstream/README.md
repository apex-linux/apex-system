# bootc upstream status fixtures

These files are bootc's own test data from
<https://github.com/bootc-dev/bootc/tree/main/crates/lib/src/fixtures>, fetched on
2026-09-24 (bootc `main` at commit `41049ccada2bf4468b9087d45832f9168d458ef2`).
bootc is licensed under **MIT OR Apache-2.0**; copyright the bootc authors.

bootc keeps them as YAML; they are converted here to the JSON that
`bootc status --format=json --format-version=1` would print for the same `Host` value:

- plain YAML scalars that YAML reads as timestamps or floats (`timestamp: 2023-09-30T19:22:16Z`,
  `version: 20231230.1`) stay strings, because those fields are strings in bootc;
- the tag `signature: !ostreeRemote "fedora"` becomes `{"ostreeRemote": "fedora"}`,
  serde's externally tagged form;
- key order and legacy keys (`isContainer`, `apiVersion: org.containers.bootc/v1alpha1`)
  are kept as they are.

`spec-v1-null.json` was already JSON and is only re-indented.

Do not edit these files by hand. Captures from the qualify VM belong in a sibling
directory named after the bootc NEVR (`bootc-<version>/`), one set per bootc version.
