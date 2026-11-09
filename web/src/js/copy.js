// The Copy buttons: a <button data-copy> copies the text of the <code> in the same .copy element. It listens on
// the document, so a button a <template> copies in later works too.
//
// The clipboard API needs a secure context, which plain HTTP on a LAN is not, so there the button selects the
// text instead, and copies it too where the browser still allows that.

function said(button, word) {
  const label = button.dataset.label ?? (button.dataset.label = button.textContent);
  button.textContent = word;
  setTimeout(() => (button.textContent = label), 2000);
}

function select(button, code) {
  const range = document.createRange();
  range.selectNodeContents(code);
  const selection = window.getSelection();
  selection.removeAllRanges();
  selection.addRange(range);
  let copied = false;
  try {
    copied = document.execCommand("copy");
  } catch (e) {
    copied = false;
  }
  said(button, copied ? "Copied" : "Selected");
}

document.addEventListener("click", (event) => {
  const button = event.target.closest("button[data-copy]");
  const code = button && button.closest(".copy")?.querySelector("code");
  if (!code) return;
  if (window.isSecureContext && navigator.clipboard) {
    navigator.clipboard.writeText(code.textContent).then(() => said(button, "Copied"), () => select(button, code));
  } else {
    select(button, code);
  }
});
