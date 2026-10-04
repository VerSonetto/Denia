import { useEffect, useRef } from 'react'
import { MAX_FILL_FRAGMENT_SHADER } from './effortMaxShader'

const VERTEX_SHADER = `
attribute vec2 aPosition;
varying vec2 vUv;
void main() {
  vUv = aPosition * 0.5 + 0.5;
  gl_Position = vec4(aPosition, 0.0, 1.0);
}`

/** 最高档的流动紫色填充。无 WebGL 时保留 CSS 渐变，关闭浮层即释放 GPU 资源。 */
export function EffortMaxFill({ reducedMotion, reveal }: { reducedMotion: boolean; reveal: boolean }) {
  const canvasRef = useRef<HTMLCanvasElement>(null)
  useEffect(() => {
    const canvas = canvasRef.current
    if (!canvas || typeof WebGLRenderingContext === 'undefined' || typeof ResizeObserver === 'undefined') return
    const gl = canvas.getContext('webgl', { alpha: true, antialias: false, depth: false, stencil: false })
    if (!gl) return
    const compile = (type: number, source: string) => {
      const shader = gl.createShader(type)
      if (!shader) return null
      gl.shaderSource(shader, source)
      gl.compileShader(shader)
      if (gl.getShaderParameter(shader, gl.COMPILE_STATUS)) return shader
      gl.deleteShader(shader)
      return null
    }
    const vertex = compile(gl.VERTEX_SHADER, VERTEX_SHADER)
    const fragment = compile(gl.FRAGMENT_SHADER, MAX_FILL_FRAGMENT_SHADER)
    const program = vertex && fragment ? gl.createProgram() : null
    if (program && vertex && fragment) {
      gl.attachShader(program, vertex)
      gl.attachShader(program, fragment)
      gl.linkProgram(program)
    }
    if (vertex) gl.deleteShader(vertex)
    if (fragment) gl.deleteShader(fragment)
    if (!program) return
    if (!gl.getProgramParameter(program, gl.LINK_STATUS)) { gl.deleteProgram(program); return }
    const buffer = gl.createBuffer()
    if (!buffer) { gl.deleteProgram(program); return }
    gl.useProgram(program)
    gl.bindBuffer(gl.ARRAY_BUFFER, buffer)
    gl.bufferData(gl.ARRAY_BUFFER, new Float32Array([-1, -1, 1, -1, -1, 1, -1, 1, 1, -1, 1, 1]), gl.STATIC_DRAW)
    const position = gl.getAttribLocation(program, 'aPosition')
    gl.enableVertexAttribArray(position)
    gl.vertexAttribPointer(position, 2, gl.FLOAT, false, 0, 0)
    const resolution = gl.getUniformLocation(program, 'uResolution')
    const time = gl.getUniformLocation(program, 'uTime')
    const started = performance.now()
    let frame = 0
    const draw = () => {
      if (gl.isContextLost()) return
      gl.uniform1f(time, reducedMotion ? 0 : (performance.now() - started) / 1000)
      gl.drawArrays(gl.TRIANGLES, 0, 6)
    }
    const tick = () => {
      frame = 0
      if (document.hidden || gl.isContextLost()) return
      draw()
      frame = requestAnimationFrame(tick)
    }
    const visibility = () => {
      cancelAnimationFrame(frame)
      frame = 0
      if (!document.hidden) {
        draw()
        if (!reducedMotion) frame = requestAnimationFrame(tick)
      }
    }
    const resize = () => {
      const { width, height } = canvas.getBoundingClientRect()
      const ratio = Math.min(window.devicePixelRatio || 1, 2)
      canvas.width = Math.max(1, Math.round(width * ratio))
      canvas.height = Math.max(1, Math.round(height * ratio))
      gl.viewport(0, 0, canvas.width, canvas.height)
      gl.uniform2f(resolution, Math.max(1, width), Math.max(1, height))
      draw()
    }
    const lost = (event: Event) => { event.preventDefault(); cancelAnimationFrame(frame); frame = 0 }
    const observer = new ResizeObserver(resize)
    resize()
    observer.observe(canvas)
    visibility()
    canvas.addEventListener('webglcontextlost', lost)
    document.addEventListener('visibilitychange', visibility)
    return () => {
      cancelAnimationFrame(frame)
      observer.disconnect()
      canvas.removeEventListener('webglcontextlost', lost)
      document.removeEventListener('visibilitychange', visibility)
      gl.deleteBuffer(buffer)
      gl.deleteProgram(program)
    }
  }, [reducedMotion])
  return <span className="effort-max-fill" data-reveal={!reducedMotion && reveal} aria-hidden="true">
    <canvas className="effort-max-canvas" ref={canvasRef} />
  </span>
}
