import { createSignal } from "solid-js";

// 阅读器 chrome（顶栏/底栏/App 导航）显隐，由点正文切换。
// App.tsx 的顶栏导航与 Reader 共用这个信号，进阅读界面后顶栏导航
// 也随底栏一起点按隐藏/显示；离开阅读界面后导航恒显示（App 侧判断）。
export const [readerChrome, setReaderChrome] = createSignal(true);
