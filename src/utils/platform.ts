// Which desktop the webview is running on.
//
// `navigator.userAgentData.platform` is the modern field but is still absent in
// the WKWebView/WebView2 versions Tauri embeds on some systems, so the legacy
// `navigator.platform` stays as the first source and the user agent as the last
// resort. Detection lives here rather than being re-sniffed per view: the same
// string was already being parsed three different ways.

const descriptor = [
  (navigator as { userAgentData?: { platform?: string } }).userAgentData?.platform,
  navigator.platform,
  navigator.userAgent
]
  .filter((value): value is string => typeof value === 'string' && value.length > 0)
  .join(' ')
  .toLowerCase()

export const isMacOS = descriptor.includes('mac')

// Checked after macOS on purpose: a Mac user agent contains neither "win" nor
// "windows", but the reverse is not something to rely on.
export const isWindows = !isMacOS && descriptor.includes('win')
