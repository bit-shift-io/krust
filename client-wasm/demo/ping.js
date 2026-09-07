export default function ping(p) {
  if (typeof fetch === 'undefined') return;
  return fetch('/demo/step?p=' + encodeURIComponent(p)).catch(() => {});
}