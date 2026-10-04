// Flowing maximum-effort fill, recovered from the installed desktop slider.
export const MAX_FILL_FRAGMENT_SHADER = `precision highp float;

  varying vec2 vUv;

  uniform float uTime;
  uniform vec2 uResolution;

  const vec3 COLOR_1 = vec3(0.592, 0.388, 0.945);
  const vec3 COLOR_2 = vec3(0.831, 0.710, 0.953);
  const vec3 COLOR_3 = vec3(0.286, 0.000, 0.404);
  const vec3 COLOR_4 = vec3(0.145, 0.055, 0.478);
  const vec3 COLOR_5 = vec3(0.592, 0.000, 0.996);
  const vec3 COLOR_6 = vec3(0.780, 0.459, 0.914);
  const vec3 COLOR_7 = vec3(0.725, 0.576, 1.000);
  const vec3 COLOR_8 = vec3(0.400, 0.212, 0.820);
  const vec3 COLOR_9 = vec3(0.882, 0.690, 1.000);
  const vec3 COLOR_10 = vec3(0.498, 0.345, 0.957);
  const vec3 COLOR_11 = vec3(0.659, 0.275, 0.910);
  const vec3 COLOR_12 = vec3(0.212, 0.063, 0.400);

  float grain(vec2 uv) {
    vec2 grainUv = uv * uResolution * 0.5;
    return fract(sin(dot(grainUv + uTime, vec2(12.9898, 78.233))) * 43758.5453) * 2.0 - 1.0;
  }

  float fieldWeight(vec2 point, vec2 center) {
    return exp(-dot(point - center, point - center) * 12.0);
  }

  vec3 fieldColor(vec2 rawUv) {
    const float speed = 1.25;
    vec2 uv = vec2(rawUv.x, 0.40 + rawUv.y * 0.18);
    vec2 spatialScale = vec2(1.55, 1.0);
    vec2 point = uv * spatialScale;
    vec2 center1 = vec2(0.18 + sin(uTime * speed * 0.42) * 0.18, 0.36 + cos(uTime * speed * 0.50) * 0.42) * spatialScale;
    vec2 center2 = vec2(0.34 + cos(uTime * speed * 0.62) * 0.24, 0.62 + sin(uTime * speed * 0.47) * 0.38) * spatialScale;
    vec2 center3 = vec2(0.52 + sin(uTime * speed * 0.38) * 0.28, 0.30 + cos(uTime * speed * 0.58) * 0.36) * spatialScale;
    vec2 center4 = vec2(0.70 + cos(uTime * speed * 0.54) * 0.24, 0.68 + sin(uTime * speed * 0.41) * 0.36) * spatialScale;
    vec2 center5 = vec2(0.88 + sin(uTime * speed * 0.74) * 0.16, 0.36 + cos(uTime * speed * 0.64) * 0.40) * spatialScale;
    vec2 center6 = vec2(0.12 + cos(uTime * speed * 0.48) * 0.20, 0.72 + sin(uTime * speed * 0.70) * 0.30) * spatialScale;
    vec2 center7 = vec2(0.30 + sin(uTime * speed * 0.58) * 0.22, 0.44 + cos(uTime * speed * 0.52) * 0.42) * spatialScale;
    vec2 center8 = vec2(0.46 + cos(uTime * speed * 0.68) * 0.26, 0.72 + sin(uTime * speed * 0.56) * 0.32) * spatialScale;
    vec2 center9 = vec2(0.60 + sin(uTime * speed * 0.44) * 0.28, 0.26 + cos(uTime * speed * 0.60) * 0.38) * spatialScale;
    vec2 center10 = vec2(0.76 + cos(uTime * speed * 0.50) * 0.22, 0.54 + sin(uTime * speed * 0.66) * 0.40) * spatialScale;
    vec2 center11 = vec2(0.92 + sin(uTime * speed * 0.70) * 0.15, 0.66 + cos(uTime * speed * 0.46) * 0.30) * spatialScale;
    vec2 center12 = vec2(0.06 + cos(uTime * speed * 0.40) * 0.14, 0.32 + sin(uTime * speed * 0.60) * 0.40) * spatialScale;
    float weight1 = fieldWeight(point, center1) * (0.7 + 0.3 * sin(uTime * 0.91));
    float weight2 = fieldWeight(point, center2) * (0.7 + 0.3 * cos(uTime * 1.07));
    float weight3 = fieldWeight(point, center3) * (0.7 + 0.3 * sin(uTime * 0.76));
    float weight4 = fieldWeight(point, center4) * (0.7 + 0.3 * cos(uTime * 1.18));
    float weight5 = fieldWeight(point, center5) * (0.7 + 0.3 * sin(uTime * 1.03));
    float weight6 = fieldWeight(point, center6) * (0.7 + 0.3 * cos(uTime * 0.83));
    float weight7 = fieldWeight(point, center7) * (0.7 + 0.3 * sin(uTime * 1.24));
    float weight8 = fieldWeight(point, center8) * (0.7 + 0.3 * cos(uTime * 0.96));
    float weight9 = fieldWeight(point, center9) * (0.7 + 0.3 * sin(uTime * 1.11));
    float weight10 = fieldWeight(point, center10) * (0.7 + 0.3 * cos(uTime * 0.72));
    float weight11 = fieldWeight(point, center11) * (0.7 + 0.3 * sin(uTime * 1.29));
    float weight12 = fieldWeight(point, center12) * (0.7 + 0.3 * cos(uTime * 0.88));
    float totalWeight = max(
      weight1 + weight2 + weight3 + weight4 + weight5 + weight6 +
        weight7 + weight8 + weight9 + weight10 + weight11 + weight12,
      0.0001
    );
    vec3 color = (
      COLOR_1 * weight1 + COLOR_2 * weight2 + COLOR_3 * weight3 +
      COLOR_4 * weight4 + COLOR_5 * weight5 + COLOR_6 * weight6 +
      COLOR_7 * weight7 + COLOR_8 * weight8 + COLOR_9 * weight9 +
      COLOR_10 * weight10 + COLOR_11 * weight11 + COLOR_12 * weight12
    ) / totalWeight;
    color = mix(COLOR_4, color, 0.96);

    return pow(clamp(color, vec3(0.0), vec3(1.0)), vec3(0.9));
  }

  void main() {
    vec3 color = fieldColor(vUv);
    color += grain(vUv) * 0.012;
    color.r += sin(uTime * 0.5) * 0.02;
    color.g += cos(uTime * 0.7) * 0.02;
    color.b += sin(uTime * 0.6) * 0.02;
    color = pow(color, vec3(0.92));

    gl_FragColor = vec4(color, 1.0);
  }
`
