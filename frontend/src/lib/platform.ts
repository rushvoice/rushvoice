// 開いている OS の判定と、キー表記の Mac 向け言い換え（2026-11-17 Mac 利用者の指摘）
//
// ホットキーの設定値は Windows の名前（NonConvert・Alt・Meta）で持っている。
// Mac の裏側（keyboard_hook/macos_impl.rs）は NonConvert を「英数」、Convert を「かな」として読むので、
// 画面の表記だけを Mac の呼び方にそろえる。

export const IS_MAC = typeof navigator !== 'undefined' && /Mac/i.test(navigator.userAgent)

const MAC_WORDS: Record<string, string> = {
  無変換: '英数',
  変換: 'かな',
  カナ: 'かな',
  'Win/Cmd': 'Cmd',
  Win: 'Cmd',
  Alt: 'Option',
}

/** 「Ctrl + Shift + 無変換」のような表記を、Mac なら「Ctrl + Shift + 英数」にする */
export function localizeKeyLabel(label: string): string {
  if (!IS_MAC) return label
  return label.replace(/無変換|変換|カナ|Win\/Cmd|Win|Alt/g, (w) => MAC_WORDS[w] ?? w)
}
