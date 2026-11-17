import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import { CanvasController, DEFAULT_SETTINGS, type Settings, type UiTool } from './controller'
import { onAppearanceChange, readAppearance, setAppearance } from '../appearance'
import type { Layers, Tool, View } from './ink'
import { isMonthKey, loadJSON, monthKey, monthLabel, monthsWithContent, prevMonth, saveJSON } from './store'
import './canvas.css'

const K_TOOLS = 'memorious.canvas.tools'
const K_LAYERS = 'memorious.canvas.layers'
const K_MONTHS = 'memorious.canvas.months'
const K_VIEW = 'memorious.canvas.view'
const K_TOUCH = 'memorious.canvas.touch'
const K_BAR = 'memorious.canvas.toolbar'

interface MonthPrefs {
  visible: string[]
  extra: string[]
}

const isDraw = (t: UiTool): t is Tool => t === 'pencil' || t === 'pen' || t === 'eraser'
const isDark = () => {
  const a = readAppearance()
  return a === 'dark' || (a === 'system' && window.matchMedia('(prefers-color-scheme: dark)').matches)
}

/** 18px line icons (stroke = currentColor). */
const Icon = ({ d }: { d: string }) => (
  <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" strokeLinejoin="round" aria-hidden>
    <path d={d} />
  </svg>
)
const ICONS = {
  undo: 'M9 14L4 9l5-5M4 9h10a6 6 0 010 12h-3',
  redo: 'M15 14l5-5-5-5M20 9H10a6 6 0 000 12h3',
  trash: 'M4 7h16M10 11v6M14 11v6M6 7l1 13h10l1-13M9 7V4h6v3',
  copy: 'M9 9h11v11H9zM5 15V4h11',
}

interface ControlsProps {
  tool: UiTool
  settings: Settings
  floating: boolean
  canUndo: boolean
  canRedo: boolean
  onTool(t: UiTool): void
  onSetting(t: Tool, s: { size?: number; op?: number }): void
  onImage(): void
  onPlace(): void
  onCancel(): void
  onUndo(): void
  onRedo(): void
  dark: boolean
  touch: boolean
  onTouch(): void
}

/** The toolbar and the right-click menu are the same controls in two containers. */
function ToolControls(p: ControlsProps) {
  const s = isDraw(p.tool) ? p.settings[p.tool] : null
  const draw = isDraw(p.tool) ? p.tool : null
  return (
    <>
      <div className="canvas-row">
        {(['pencil', 'pen', 'eraser', 'lasso', 'rect'] as UiTool[]).map((t) => (
          <button key={t} className={'canvas-btn' + (p.tool === t ? ' on' : '')} onClick={() => p.onTool(t)}>
            {t}
          </button>
        ))}
        {p.floating ? (
          <>
            <button className="canvas-btn on" onClick={p.onPlace}>
              place
            </button>
            <button className="canvas-btn" onClick={p.onCancel}>
              cancel
            </button>
          </>
        ) : (
          <button className="canvas-btn" onClick={p.onImage}>
            image
          </button>
        )}
      </div>
      {s && draw && (
        <>
      <label className="canvas-slider">
        <span>size</span>
        <input
          type="range"
          min={1}
          max={24}
          step={0.5}
          value={s.size}
          onChange={(e) => p.onSetting(draw, { size: +e.target.value })}
        />
        <b>{s.size}</b>
      </label>
      <label className="canvas-slider">
        <span>opacity</span>
        <input
          type="range"
          min={0.05}
          max={1}
          step={0.05}
          value={s.op}
          onChange={(e) => p.onSetting(draw, { op: +e.target.value })}
        />
        <b>{s.op.toFixed(2)}</b>
      </label>
        </>
      )}
      <div className="canvas-row">
        <button className="canvas-btn icon" title="undo" aria-label="undo" disabled={!p.canUndo} onClick={p.onUndo}>
          <Icon d={ICONS.undo} />
        </button>
        <button className="canvas-btn icon" title="redo" aria-label="redo" disabled={!p.canRedo} onClick={p.onRedo}>
          <Icon d={ICONS.redo} />
        </button>
        <button className={'canvas-btn' + (!p.dark ? ' on' : '')} onClick={() => setAppearance('light')}>
          light
        </button>
        <button className={'canvas-btn' + (p.dark ? ' on' : '')} onClick={() => setAppearance('dark')}>
          dark
        </button>
        <button className={'canvas-btn' + (p.touch ? ' on' : '')} onClick={p.onTouch}>
          finger
        </button>
      </div>
    </>
  )
}

export function CanvasView() {
  const host = useRef<HTMLDivElement>(null)
  const file = useRef<HTMLInputElement>(null)
  const ctl = useRef<CanvasController | null>(null)
  const now = useMemo(() => monthKey(), [])
  const prev = useMemo(() => prevMonth(now), [now])

  const [tool, setToolState] = useState<UiTool>('pencil')
  const [dark, setDark] = useState(isDark)
  const [touch, setTouch] = useState<boolean>(() => {
    const v = localStorage.getItem(K_TOUCH)
    return v ? v === 'on' : window.matchMedia('(pointer: coarse)').matches // phones draw with a finger by default
  })
  const [barOpen, setBarOpen] = useState(() => localStorage.getItem(K_BAR) !== 'closed')
  const [corner, setCorner] = useState<{ x: number; y: number } | null>(null)
  const [settings, setSettings] = useState<Settings>(() => ({ ...DEFAULT_SETTINGS, ...loadJSON<Partial<Settings>>(K_TOOLS, {}) }))
  const [layers, setLayers] = useState<Layers>(() => loadJSON(K_LAYERS, { pencil: true, pen: true, image: true }))
  const [prefs, setPrefs] = useState<MonthPrefs>(() => loadJSON(K_MONTHS, { visible: [prev], extra: [] }))
  const [floating, setFloating] = useState(false)
  const [menu, setMenu] = useState<{ x: number; y: number } | null>(null)
  const [tick, setTick] = useState(0)

  // controller lifetime = view lifetime
  useEffect(() => {
    const c = new CanvasController(
      host.current!,
      {
        onChange: () => {
          setTick((t) => t + 1)
          saveJSON(K_VIEW, c.view)
        },
        onFloating: setFloating,
        onSelection: setCorner,
        onTouchDraws: (on) => setTouch(on),
        onContext: (x, y) => setMenu({ x, y }),
      },
      { settings, view: loadJSON<View>(K_VIEW, { x: 0, y: 0, z: 1 }), touchDraws: touch },
    )
    ctl.current = c
    c.layers = layers
    c.visibleMonths = prefs.visible.filter((m) => m !== now)
    c.setTool('pencil')
    c.rebuild()
    return () => {
      c.dispose()
      ctl.current = null
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [])

  const months = useMemo(() => {
    const all = new Set([now, prev, ...monthsWithContent(), ...prefs.extra])
    return [...all].sort().reverse()
    // tick: a stroke may have made the current month non-empty
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [now, prev, prefs.extra, tick])

  useEffect(() => {
    const f = () => setDark(isDark())
    const m = window.matchMedia('(prefers-color-scheme: dark)')
    m.addEventListener('change', f)
    const off = onAppearanceChange(f)
    return () => {
      m.removeEventListener('change', f)
      off()
    }
  }, [])

  const pickTool = useCallback((t: UiTool) => {
    setToolState(t)
    ctl.current?.setTool(t)
  }, [])
  const onSetting = useCallback((t: Tool, s: { size?: number; op?: number }) => {
    ctl.current?.setSetting(t, s)
    setSettings((cur) => {
      const next = { ...cur, [t]: { ...cur[t], ...s } }
      saveJSON(K_TOOLS, next)
      return next
    })
  }, [])
  const toggleLayer = (l: keyof Layers) => {
    const next = { ...layers, [l]: !layers[l] }
    setLayers(next)
    saveJSON(K_LAYERS, next)
    ctl.current?.setLayers(next)
  }
  const toggleMonth = (m: string) => {
    const visible = prefs.visible.includes(m) ? prefs.visible.filter((x) => x !== m) : [...prefs.visible, m]
    const next = { ...prefs, visible }
    setPrefs(next)
    saveJSON(K_MONTHS, next)
    ctl.current?.setVisibleMonths(visible)
  }
  const addMonth = (m: string) => {
    if (!isMonthKey(m) || m === now) return
    const next = { extra: [...new Set([...prefs.extra, m])], visible: [...new Set([...prefs.visible, m])] }
    setPrefs(next)
    saveJSON(K_MONTHS, next)
    ctl.current?.setVisibleMonths(next.visible)
  }

  // close the context menu on any outside press / Escape
  useEffect(() => {
    if (!menu) return
    const close = (e: Event) => {
      if (!(e.target as HTMLElement).closest?.('.canvas-menu')) setMenu(null)
    }
    const esc = (e: KeyboardEvent) => e.key === 'Escape' && setMenu(null)
    window.addEventListener('pointerdown', close, true)
    window.addEventListener('keydown', esc)
    return () => {
      window.removeEventListener('pointerdown', close, true)
      window.removeEventListener('keydown', esc)
    }
  }, [menu])

  const controls = (): ControlsProps => ({
    tool,
    settings,
    floating,
    canUndo: ctl.current?.canUndo ?? false,
    canRedo: ctl.current?.canRedo ?? false,
    onTool: (t) => pickTool(t),
    onSetting,
    onImage: () => {
      setMenu(null)
      file.current?.click()
    },
    onPlace: () => ctl.current?.placeImage(),
    onCancel: () => ctl.current?.cancelImage(),
    onUndo: () => ctl.current?.undo(),
    onRedo: () => ctl.current?.redo(),
    dark,
    touch,
    onTouch: () => {
      const next = !touch
      setTouch(next)
      localStorage.setItem(K_TOUCH, next ? 'on' : 'off')
      if (ctl.current) ctl.current.touchDraws = next
    },
  })

  return (
    <div className="canvas-view">
      <div className="canvas-host" ref={host} />

      {barOpen && (
      <aside className="canvas-layers">
        {(['pencil', 'pen', 'image'] as (keyof Layers)[]).map((l) => (
          <label key={l} className="canvas-check">
            <input type="checkbox" checked={layers[l]} onChange={() => toggleLayer(l)} />
            {l === 'image' ? 'images' : l}
          </label>
        ))}
        <hr />
        {months.map((m) => (
          <label key={m} className="canvas-check">
            <input
              type="checkbox"
              checked={m === now || prefs.visible.includes(m)}
              disabled={m === now}
              onChange={() => toggleMonth(m)}
            />
            {monthLabel(m)}
          </label>
        ))}
        <input
          className="canvas-month-pick"
          type="month"
          aria-label="add month"
          max={now}
          onChange={(e) => {
            addMonth(e.target.value)
            e.target.value = ''
          }}
        />
      </aside>
      )}

      {barOpen && (
        <div className="canvas-toolbar">
          <ToolControls {...controls()} />
        </div>
      )}
      <button
        className={'canvas-dot' + (barOpen ? ' open' : '')}
        aria-label="toolbar"
        onClick={() => {
          setBarOpen(!barOpen)
          localStorage.setItem(K_BAR, barOpen ? 'closed' : 'open')
        }}
      />

      {corner && (
        <div className="canvas-selbar" style={{ left: Math.max(corner.x, 100), top: Math.max(corner.y, 54) }}>
          <button className="canvas-btn icon" title="duplicate" aria-label="duplicate" onClick={() => ctl.current?.duplicateSelection()}>
            <Icon d={ICONS.copy} />
          </button>
          <button className="canvas-btn icon" title="delete" aria-label="delete" onClick={() => ctl.current?.deleteSelection()}>
            <Icon d={ICONS.trash} />
          </button>
        </div>
      )}

      {menu && (
        <div className="canvas-menu" style={{ left: Math.min(menu.x, window.innerWidth - 240), top: Math.min(menu.y, window.innerHeight - 220) }}>
          <ToolControls {...controls()} />
        </div>
      )}

      <input
        ref={file}
        type="file"
        accept="image/*"
        hidden
        onChange={(e) => {
          const f = e.target.files?.[0]
          if (f) void ctl.current?.addImage(f)
          e.target.value = ''
        }}
      />
    </div>
  )
}
