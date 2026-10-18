#!/usr/bin/env python3
"""Reduce a pinned sapics user-country IPv4 CSV to offline localization hints."""
import argparse
import csv
import hashlib
import ipaddress
import json
import tempfile
import urllib.request
from pathlib import Path


def public_ipv4(address):
    return (address.version == 4 and address.is_global and not address.is_reserved
            and not address.is_multicast and not address.is_loopback
            and not address.is_link_local)


def select_countries(rows):
    """Choose an interior IP in the largest eligible range; tie-break by start."""
    candidates = {}
    for row in rows:
        if len(row) != 3:
            raise ValueError('Expected start IP, end IP, country code in every CSV row')
        start_text, end_text, country = row
        country = country.strip().lower()
        if (len(country) != 2 or not country.isascii() or not country.isalpha()
                or country in {'zz', 'xx', 'eu', 'ap'}):
            continue
        start, end = ipaddress.IPv4Address(start_text), ipaddress.IPv4Address(end_text)
        if int(end) < int(start):
            raise ValueError('IP range ends before it starts')
        if not public_ipv4(start) or not public_ipv4(end):
            continue
        midpoint = (int(start) + int(end)) // 2
        # Avoid conventional /24 network/broadcast suffixes when possible.
        if midpoint % 256 == 0:
            midpoint += 1
        elif midpoint % 256 == 255:
            midpoint += 2
        if midpoint > int(end):
            continue
        ip = ipaddress.IPv4Address(midpoint)
        if not public_ipv4(ip):
            continue
        score = (int(end) - int(start) + 1, -int(start))
        if country not in candidates or score > candidates[country][0]:
            candidates[country] = (score, str(ip))
    return {country: candidates[country][1] for country in sorted(candidates)}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--source', type=Path)
    parser.add_argument('--source-metadata', type=Path,
                        help='GitHub release asset metadata captured at download time')
    parser.add_argument('--download', action='store_true', help='Download current source and release metadata at generation time')
    parser.add_argument('--generated-on', required=True, help='YYYY-MM-DD')
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix='proviz-country-ips-') as directory:
        if args.download:
            if args.source or args.source_metadata:
                parser.error('--download cannot be combined with local source options')
            headers = {'User-Agent': 'proviz-sercilo-country-defaults'}
            request = urllib.request.Request(
                'https://api.github.com/repos/sapics/ip-location-db/releases/tags/latest', headers=headers)
            with urllib.request.urlopen(request, timeout=45) as response:
                release = json.load(response)
            metadata = next(a for a in release['assets'] if a['name'] == 'user-country-ipv4.csv')
            metadata = {key: metadata[key] for key in
                        ['id', 'name', 'size', 'updated_at', 'digest', 'browser_download_url']}
            metadata.update(release_id=release['id'], release_tag=release['tag_name'])
            args.source = Path(directory) / 'user-country-ipv4.csv'
            request = urllib.request.Request(metadata['browser_download_url'], headers=headers)
            with urllib.request.urlopen(request, timeout=60) as response:
                args.source.write_bytes(response.read())
            generate(args, metadata)
            # Persist the exact source version next to the generated snapshot.
            args.output.with_suffix('.source.json').write_text(json.dumps(metadata, indent=2) + '\n')
        else:
            if not args.source or not args.source_metadata:
                parser.error('supply --download or both --source and --source-metadata')
            generate(args, json.loads(args.source_metadata.read_text()))


def generate(args, metadata):
    digest = hashlib.sha256(args.source.read_bytes()).hexdigest()
    if metadata.get('digest') != 'sha256:' + digest:
        raise ValueError('CSV checksum does not match the pinned GitHub asset digest')
    with args.source.open(newline='') as source:
        countries = select_countries(csv.reader(source))
    if len(countries) < 200:
        raise ValueError('Expected coverage of at least 200 countries/territories')
    document = {
        'source': 'sapics/ip-location-db user-country IPv4',
        'source_url': metadata['browser_download_url'],
        'source_release_id': metadata['release_id'],
        'source_release_tag': metadata['release_tag'],
        'source_asset_id': metadata['id'],
        'source_updated_at': metadata['updated_at'],
        'source_sha256': digest,
        'license': 'PDDL-1.0',
        'license_url': 'https://opendatacommons.org/licenses/pddl/1-0/',
        'generated_on': args.generated_on,
        'selection': 'Interior IPv4 in the largest public candidate range per country; ties use lowest range start. Unknown and regional codes excluded.',
        'country_ips': countries,
    }
    args.output.write_text(json.dumps(document, indent=2) + '\n')
    print(f'Generated {len(countries)} country/territory IP hints')


if __name__ == '__main__':
    main()
