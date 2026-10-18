import unittest
import tempfile
from pathlib import Path
from types import SimpleNamespace
from generate_country_ips import generate, select_countries


class CountrySelectionTests(unittest.TestCase):
    def test_largest_range_wins_and_row_order_does_not_matter(self):
        rows = [
            ['80.0.0.0', '80.0.0.255', 'FR'],
            ['90.0.0.0', '90.0.255.255', 'FR'],
            ['81.0.0.0', '81.0.255.255', 'FR'],
        ]
        self.assertEqual(select_countries(rows), {'fr': '81.0.128.1'})
        self.assertEqual(select_countries(reversed(rows)), select_countries(rows))

    def test_private_reserved_multicast_and_unknown_ranges_are_excluded(self):
        rows = [[start, end, 'FR'] for start, end in [
            ('10.0.0.0', '10.255.255.255'),
            ('127.0.0.0', '127.255.255.255'),
            ('192.0.2.0', '192.0.2.255'),
            ('224.0.0.0', '224.0.0.255'),
            ('240.0.0.0', '240.0.0.255'),
        ]]
        rows += [['80.0.0.0', '80.0.0.255', code] for code in ['ZZ', 'EU', '', 'FRANCE']]
        self.assertEqual(select_countries(rows), {})

    def test_avoids_network_suffix_and_preserves_single_public_ip(self):
        self.assertEqual(select_countries([
            ['80.0.0.0', '80.0.1.255', 'FR'],
            ['81.0.0.42', '81.0.0.42', 'GB'],
        ]), {'fr': '80.0.1.1', 'gb': '81.0.0.42'})

    def test_checksum_mismatch_preserves_existing_output(self):
        with tempfile.TemporaryDirectory() as directory:
            source = Path(directory) / 'source.csv'
            output = Path(directory) / 'defaults.json'
            source.write_text('80.0.0.0,80.0.0.255,FR\n')
            output.write_text('original')
            args = SimpleNamespace(source=source, output=output)
            with self.assertRaisesRegex(ValueError, 'checksum'):
                generate(args, {'digest': 'sha256:incorrect'})
            self.assertEqual(output.read_text(), 'original')

    def test_reversed_or_malformed_ranges_fail(self):
        for rows in [[['80.0.0.2', '80.0.0.1', 'FR']], [['80.0.0.1']]]:
            with self.assertRaises(ValueError):
                select_countries(rows)


if __name__ == '__main__':
    unittest.main()
