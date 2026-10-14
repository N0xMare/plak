"""UUID constructors require features declared by their own package."""

import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / 'tools/checks'))
import check_features


class UuidFeatureTests(unittest.TestCase):
    def findings(self, manifest, sources, inherited=None):
        return check_features.uuid_feature_findings(manifest, inherited or {}, sources)

    def test_missing_v4_is_not_supplied_by_another_workspace_member(self):
        manifest = {'dev-dependencies': {'uuid': {'workspace': True}}}
        self.assertEqual(self.findings(manifest, {'tests/test.rs': 'uuid::Uuid::new_v4()'}),
                         ['tests/test.rs: UUID constructor new_v4 requires uuid feature v4 '
                          'in this package'])

    def test_generation_features_are_checked_including_inline_tests(self):
        source = '#[cfg(test)] mod tests { use uuid::Uuid; fn test() { Uuid::now_v7(); } }'
        self.assertEqual(len(self.findings({'dependencies': {'uuid': '1'}},
                                           {'src/lib.rs': source})), 1)
        for version in range(1, 9):
            with self.subTest(version=version):
                manifest = {'dependencies': {'uuid': {'features': [f'v{version}']}}}
                self.assertEqual(self.findings(manifest, {
                    'src/lib.rs': f'Uuid::new_v{version}()',
                }), [])

    def test_local_and_inherited_features_are_additive(self):
        manifest = {'dev-dependencies': {'uuid': {'workspace': True, 'features': ['v4']}}}
        inherited = {'uuid': {'version': '1', 'features': ['v7']}}
        self.assertEqual(self.findings(manifest, {
            'tests/test.rs': 'Uuid::new_v4(); Uuid::now_v7();',
        }, inherited), [])

    def test_renamed_uuid_dependency_and_type_are_checked(self):
        manifest = {'dependencies': {'ids': {'package': 'uuid', 'features': ['v7']}}}
        source = 'use ids::Uuid as Id; Id::new_v4();'
        self.assertEqual(len(self.findings(manifest, {'src/lib.rs': source})), 1)

    def test_comments_and_literals_are_not_feature_uses(self):
        source = '''// Uuid::new_v4()
/* nested /* Uuid::new_v5() */ Uuid::now_v7() */
const TEXT: &str = r#"Uuid::new_v4()"#;
const OTHER: &str = "Uuid::new_v6()";
Uuid::from_u128(7);
'''
        self.assertEqual(self.findings({}, {'src/lib.rs': source}), [])

    def test_unused_uuid_does_not_require_generation_features(self):
        self.assertEqual(self.findings({'dependencies': {'uuid': '1'}}, {
            'src/lib.rs': 'use uuid::Uuid; Uuid::parse_str("abc");',
        }), [])


if __name__ == '__main__':
    unittest.main()
