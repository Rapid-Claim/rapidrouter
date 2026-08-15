import { render } from "solid-js/web";
import { App } from "./app";
import "uplot/dist/uPlot.min.css";
import "./tokens.css";
import "./styles.css";

render(() => <App />, document.getElementById("root")!);
