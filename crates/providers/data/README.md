# Bundled country localization hints

`country_ips.json` contains one public IPv4 address for each of 250 countries and
territories, including the dataset's `xk` code. It is compiled into the provider
binary; search requests never download this data or contact the selected IP.

Source: [sapics/ip-location-db, user-country IPv4](https://github.com/sapics/ip-location-db).
The source publisher distributes this dataset under
[PDDL 1.0](https://opendatacommons.org/licenses/pddl/1-0/).
`country_ips.source.json` records the GitHub release asset metadata. The generated
snapshot includes its source asset ID, update timestamp, SHA-256, license and
generation date. The mutable `latest` URL alone is not a version identifier.

Generation picks an interior public IPv4 address from the largest eligible range
for each country. Equal-size ranges use the lowest starting address. Unknown and
regional codes, private, reserved and multicast addresses are excluded. Suffixes
`.0` and `.255` are avoided. The generator validates the source checksum before
writing the defaults.

These are dataset-derived country hints, not independently verified locations or
a promise that DeepSeek's geolocation database agrees. Public IP classification
does not prove a route is currently announced. Refresh the snapshot when preparing
releases. Configured country overrides take precedence; unknown countries omit
the hint. Language alone never chooses a country.

## Refresh

From the repository root, download the current dataset and generate defaults:

```sh
python3 scripts/generate_country_ips.py --download --generated-on YYYY-MM-DD \
  --output crates/providers/data/country_ips.json
```

Review both JSON files and run the generator and provider tests before committing.
Network access is needed only for this maintenance command.

To reproduce an existing snapshot using an archived CSV and the committed asset
metadata (the CSV must match the recorded SHA-256):

```sh
python3 scripts/generate_country_ips.py --source /path/to/user-country-ipv4.csv \
  --source-metadata crates/providers/data/country_ips.source.json \
  --generated-on 2026-10-18 --output /tmp/country_ips.json
cmp crates/providers/data/country_ips.json /tmp/country_ips.json
```

The current CSV is about 9 MB and is not bundled. GitHub may remove old release
assets; reproducing an old snapshot requires retaining that exact CSV separately.
The generated country map and its provenance remain in Git history.

```sh
python3 -m unittest discover -s scripts -p 'test_generate_country_ips.py'
cargo test -p providers
```
