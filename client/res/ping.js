export default function ping(p) {
  if (typeof fetch === 'undefined') return;
  return fetch('/res/step?p=' + encodeURIComponent(p)).catch(() => {});
}