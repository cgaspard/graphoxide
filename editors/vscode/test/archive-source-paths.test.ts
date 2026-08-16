import assert from 'node:assert/strict';
import test from 'node:test';
import { archiveEmbeddedSource } from '../src/graph';

test('detects nested archive-embedded sources', () => {
  assert.deepEqual(
    archiveEmbeddedSource('charts/safeevac-azure/safeevac-0.0.13.tgz!/safeevac-0.0.13.tar!/safeevac/templates/services.yml'),
    {
      outer: 'charts/safeevac-azure/safeevac-0.0.13.tgz',
      member: 'safeevac-0.0.13.tar!/safeevac/templates/services.yml',
    },
  );
});

test('detects a single-level zip member', () => {
  assert.deepEqual(archiveEmbeddedSource('bundle.zip!/member.ts'), { outer: 'bundle.zip', member: 'member.ts' });
});

test('detects every container extension the extractor indexes', () => {
  for (const extension of ['tar', 'tgz', 'gz', 'bz2', 'tbz', 'tbz2', 'xz', 'txz', 'zst', 'zstd', '7z', 'rar', 'zip', 'docx', 'xlsx', 'pptx', 'vsdx', 'odt', 'ods', 'odp', 'epub']) {
    assert.deepEqual(archiveEmbeddedSource(`bundle.${extension}!/member.txt`), { outer: `bundle.${extension}`, member: 'member.txt' });
  }
});

test('detects container extensions case-insensitively', () => {
  assert.deepEqual(archiveEmbeddedSource('RELEASE.TGZ!/x.yml'), { outer: 'RELEASE.TGZ', member: 'x.yml' });
});

test('normalizes backslash spellings before matching', () => {
  assert.deepEqual(archiveEmbeddedSource('bundle.tgz!\\inner.tar!\\a.yml'), { outer: 'bundle.tgz', member: 'inner.tar!/a.yml' });
});

test('ordinary relative paths are not archive-embedded', () => {
  assert.equal(archiveEmbeddedSource('src/auth.ts'), undefined);
  assert.equal(archiveEmbeddedSource('README.md'), undefined);
  assert.equal(archiveEmbeddedSource(''), undefined);
});

test('on-disk paths that merely contain a bang are not archive-embedded', () => {
  assert.equal(archiveEmbeddedSource('notes!draft.txt'), undefined);
  assert.equal(archiveEmbeddedSource('docs/TODO!final.md'), undefined);
  assert.equal(archiveEmbeddedSource('!leading.txt'), undefined);
});

test('a bang after a non-container extension is not archive-embedded', () => {
  assert.equal(archiveEmbeddedSource('report.md!weird'), undefined);
  assert.equal(archiveEmbeddedSource('noext!member'), undefined);
});

test('an empty member is not archive-embedded', () => {
  assert.equal(archiveEmbeddedSource('bundle.tgz!'), undefined);
});
