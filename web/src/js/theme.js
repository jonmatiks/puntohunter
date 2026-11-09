// Applies the theme the header's button saved before the page paints, so a saved choice never flashes the
// default. Without a saved choice, or without storage, the page is dark.
try {
  const theme = window.localStorage.getItem("puntohunter.theme");
  if (theme === "light" || theme === "auto") document.documentElement.dataset.theme = theme;
} catch (e) {
  // No storage: dark.
}
