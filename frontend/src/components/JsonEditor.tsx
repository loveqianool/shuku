import { createEffect, onCleanup, type Component } from "solid-js";
import { EditorView, basicSetup } from "codemirror";
import { EditorState } from "@codemirror/state";
import { json } from "@codemirror/lang-json";

interface Props {
  value: string;
  onChange: (v: string) => void;
  /** 切换编辑对象时传不同 key（父组件用 key 属性重挂载即可） */
}

/** CodeMirror 6 JSON 编辑器：用于书源 JSON 编辑。 */
const JsonEditor: Component<Props> = (props) => {
  let host: HTMLDivElement | undefined;
  let view: EditorView | undefined;

  createEffect(() => {
    if (!host) return;
    view = new EditorView({
      state: EditorState.create({
        doc: props.value,
        extensions: [
          basicSetup,
          json(),
          EditorView.updateListener.of((u) => {
            if (u.docChanged) props.onChange(u.state.doc.toString());
          }),
        ],
      }),
      parent: host,
    });
    onCleanup(() => view?.destroy());
  });

  createEffect(() => {
    const v = props.value;
    if (view && view.state.doc.toString() !== v) {
      view.dispatch({ changes: { from: 0, to: view.state.doc.length, insert: v } });
    }
  });

  return <div ref={host} />;
};

export default JsonEditor;
