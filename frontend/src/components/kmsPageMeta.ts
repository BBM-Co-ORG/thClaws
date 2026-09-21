/// Frontmatter as the KMS viewer needs it — see `KmsTrustStrip`. Kept out of
/// the component file so that file exports only components (fast refresh).

export type PageMeta = Record<string, string>;

/// Top-level `key: value` pairs of a YAML block. Enough for the strip: it
/// reads scalars and counts the items of a flow list; a block scalar or a
/// nested map is somebody's own key and is left alone.
export function parsePageMeta(yaml: string): PageMeta {
  const out: PageMeta = {};
  for (const line of yaml.split("\n")) {
    if (!line || /^[\s#-]/.test(line)) continue;
    const i = line.indexOf(":");
    if (i <= 0) continue;
    const key = line.slice(0, i).trim();
    let val = line.slice(i + 1).trim();
    if (val.length >= 2 && /^["']/.test(val) && val.endsWith(val[0])) {
      val = val.slice(1, -1);
    }
    out[key] = val;
  }
  return out;
}
