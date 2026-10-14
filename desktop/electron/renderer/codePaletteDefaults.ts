// Default code-highlight palettes (Xcode Default Light/Dark, 2026-10-04 方案:
// docs/superpowers/specs/2026-10-04-code-palette-config-design.md).
// Lives at renderer root next to themes.ts — contrib/ is linted hex-free
// (__contrib_registry.test.tsx), and palettes are definition data, not UI code.

export type CodeTokenKey =
  | 'comment' | 'keyword' | 'string' | 'number' | 'link' | 'meta'
  | 'type' | 'member' | 'other-type' | 'diff-add' | 'diff-del';
export type CodePaletteMode = 'light' | 'dark';
export type CodePalette = Partial<Record<CodeTokenKey, string>>;

/** Mirrors styles.css :root --code-* (Xcode Default Light, 2026-10-04 方案色表). */
export const DEFAULT_LIGHT: Record<CodeTokenKey, string> = {
  comment: '#707F8C',
  keyword: '#AD3DA4',
  string: '#C41A16',
  number: '#1C00CF',
  link: '#1638E6',
  meta: '#78492A',
  type: '#3F6E9E',
  member: '#3A5F8E',
  'other-type': '#A545D6',
  'diff-add': '#1F8A3D',
  'diff-del': '#C43C3C',
};

/** Mirrors themes.ts DARK (Xcode Default Dark 初稿). */
export const DEFAULT_DARK: Record<CodeTokenKey, string> = {
  comment: '#6C7986',
  keyword: '#FC5FA3',
  string: '#FC6A5D',
  number: '#D0BF69',
  link: '#6CB6FF',
  meta: '#FD8F3F',
  type: '#5DD8FF',
  member: '#4FB4FF',
  'other-type': '#BF8FFF',
  'diff-add': '#4ade80',
  'diff-del': '#f87171',
};
