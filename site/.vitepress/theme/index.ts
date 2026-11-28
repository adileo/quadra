import DefaultTheme from "vitepress/theme";
import type { Theme } from "vitepress";
import { defineAsyncComponent, h } from "vue";
import { useData } from "vitepress";
import HeroCode from "./HeroCode.vue";
import HeroCodeOps from "./HeroCodeOps.vue";
import SandboxLink from "./SandboxLink.vue";
import AppMock from "./AppMock.vue";
import Fold from "./Fold.vue";
import ScreensLive from "./ScreensLive.vue";
import AgentPrompt from "./AgentPrompt.vue";
import "./custom.css";

// the hero's code window: the home's, unless the page asks for another (`heroCode: ops`, the home; archive/home-v1.md used the default)
const HeroImage = {
  setup() {
    const { frontmatter } = useData();
    return () => h(frontmatter.value.heroCode === "ops" ? HeroCodeOps : HeroCode);
  },
};

export default {
  extends: DefaultTheme,
  Layout: () => h(DefaultTheme.Layout, null, { "home-hero-image": () => h(HeroImage) }),
  enhanceApp({ app }) {
    app.component("SandboxLink", SandboxLink);
    app.component("AppMock", AppMock);
    app.component("Fold", Fold);
    app.component("ScreensLive", ScreensLive);
    app.component("AgentPrompt", AgentPrompt);
    // the sandbox is heavy (editor + engine): loaded only on its page
    app.component("Sandbox", defineAsyncComponent(() => import("./sandbox/Sandbox.vue")));
  },
} satisfies Theme;
