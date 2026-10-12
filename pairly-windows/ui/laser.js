// The phone moves the dot by fractions of the screen; it starts in the middle.
const dot = document.getElementById("dot");
let x = 0.5, y = 0.5;
const place = () => {
  dot.style.transform = `translate(${x * innerWidth}px, ${y * innerHeight}px)`;
};
place();
// Opened by the first "show", which may arrive before this page listens.
if (new URLSearchParams(location.search).has("on")) dot.classList.add("on");
window.__TAURI__.event.listen("laser", ({ payload: [action, dx, dy] }) => {
  if (action === "show") {
    x = 0.5; y = 0.5;
    place();
    dot.classList.add("on");
  } else if (action === "move") {
    dot.classList.add("on");
    x = Math.min(1, Math.max(0, x + dx));
    y = Math.min(1, Math.max(0, y + dy));
    place();
  } else if (action === "hide") {
    dot.classList.remove("on");
  }
});
