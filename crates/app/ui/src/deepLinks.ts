const TRANSLATOR_ID = /^upload-[A-Za-z0-9_-]{1,64}$/;

export function parseTranslatorInstallUrl(value: string): string | null {
  let url: URL;
  try {
    url = new URL(value);
  } catch {
    return null;
  }
  if (
    url.protocol !== "starvault:" ||
    url.hostname !== "install" ||
    url.username ||
    url.password ||
    url.port ||
    url.search ||
    url.hash
  ) {
    return null;
  }
  const match = /^\/translator\/([^/]+)$/.exec(url.pathname);
  return match && TRANSLATOR_ID.test(match[1]) ? match[1] : null;
}
