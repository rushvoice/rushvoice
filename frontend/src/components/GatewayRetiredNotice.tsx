// GatewayRetiredNotice.tsx — 旧版の有料アカウント (RushVoice アカウント) 終了の案内 (2026-11-15)
//
// 旧版でログインしていた利用者は、更新後の初回起動でログイン情報が消える (Rust 側 endpoints::retire_gateway_login)。
// そのとき「自分の API キーか接続先を設定すれば今までどおり使える」ことを伝え、設定画面へ案内する。
// 閉じたら以後は出さない (settings_kv の印を "0" にする)。

import { useEffect, useState } from 'react'
import { Info, X, Settings } from 'lucide-react'
import { getGatewayRetiredNotice, dismissGatewayRetiredNotice } from '../lib/tauri'

export function GatewayRetiredNotice({ onOpenSettings }: { onOpenSettings?: () => void }) {
  const [show, setShow] = useState(false)

  useEffect(() => {
    let mounted = true
    getGatewayRetiredNotice()
      .then((v) => {
        if (mounted) setShow(v)
      })
      .catch((e) => console.warn('getGatewayRetiredNotice failed:', e))
    return () => {
      mounted = false
    }
  }, [])

  if (!show) return null

  async function dismiss() {
    setShow(false)
    try {
      await dismissGatewayRetiredNotice()
    } catch (e) {
      console.warn('dismissGatewayRetiredNotice failed:', e)
    }
  }

  return (
    <div
      role="status"
      className="w-full max-w-2xl flex items-start gap-3 p-4 bg-sky-950/60 border border-sky-800 rounded-lg text-sm text-sky-100"
    >
      <Info size={16} className="mt-0.5 shrink-0 text-sky-300" />
      <div className="flex-1 space-y-2">
        <p className="font-medium">RushVoice アカウント (有料版) は終了しました</p>
        <p className="text-xs text-sky-200/80 leading-relaxed">
          ログインは不要になりました。設定の「接続先と API キー」で、自分の Groq か OpenAI の API キー
          (Groq は無料枠あり) を入れるか、手元のローカル LLM などの接続先を選ぶと、今までどおりすべての機能が使えます。
        </p>
        {onOpenSettings && (
          <button
            type="button"
            onClick={onOpenSettings}
            className="inline-flex items-center gap-1.5 px-3 py-1.5 bg-sky-700 hover:bg-sky-600 text-white text-xs rounded-md"
          >
            <Settings size={12} />
            <span>設定を開く</span>
          </button>
        )}
      </div>
      <button
        type="button"
        onClick={dismiss}
        className="p-1 text-sky-300 hover:text-white"
        aria-label="案内を閉じる"
      >
        <X size={14} />
      </button>
    </div>
  )
}
