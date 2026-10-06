import * as THREE from 'three'

export function spherePosition(latitude: number, longitude: number, radius = 1): THREE.Vector3 {
  const lat = THREE.MathUtils.degToRad(latitude)
  const lon = THREE.MathUtils.degToRad(longitude)
  return new THREE.Vector3(
    radius * Math.cos(lat) * Math.sin(lon),
    radius * Math.sin(lat),
    radius * Math.cos(lat) * Math.cos(lon),
  )
}

export function sphericalArc(
  start: THREE.Vector3,
  end: THREE.Vector3,
  radius: number,
  angularStep: number,
): THREE.Vector3[] {
  const from = start.clone().normalize()
  const to = end.clone().normalize()
  const angle = from.angleTo(to)
  if (angle <= 1e-10)
    return [from.multiplyScalar(radius)]
  let axis = new THREE.Vector3().crossVectors(from, to)
  if (axis.lengthSq() <= 1e-20) {
    axis.crossVectors(from, new THREE.Vector3(0, 1, 0))
    if (axis.lengthSq() <= 1e-20)
      axis.crossVectors(from, new THREE.Vector3(1, 0, 0))
  }
  axis.normalize()
  const segments = Math.max(1, Math.ceil(angle / angularStep))
  return Array.from({ length: segments + 1 }, (_, index) =>
    from.clone().applyAxisAngle(axis, angle * index / segments).multiplyScalar(radius))
}
