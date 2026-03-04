import './styles/global.css';
import { mount } from "svelte";
import App from "./App.svelte";

// Disable the default browser right-click menu across the entire app.
document.addEventListener('contextmenu', (e) => e.preventDefault());

const app = mount(App, {
  target: document.getElementById("app")!,
});

export default app;
