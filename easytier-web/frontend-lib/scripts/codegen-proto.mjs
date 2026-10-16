import { spawnSync } from 'node:child_process'
import { existsSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, renameSync, rmSync, statSync, writeFileSync } from 'node:fs'
import { createRequire } from 'node:module'
import { delimiter, dirname, relative, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

const require = createRequire(import.meta.url)
const root = resolve(dirname(fileURLToPath(import.meta.url)), '..')
const protoRoot = resolve(root, '../../easytier-proto/proto')
const generatedRoot = resolve(root, 'src/generated')
const outDir = resolve(generatedRoot, 'proto')
const nodeBinDir = resolve(root, 'node_modules/.bin')

const protocWrapper = require.resolve('@protobuf-ts/protoc/protoc.js')
const protobufTsPluginRoot = dirname(require.resolve('@protobuf-ts/plugin/package.json'))
// Use the descriptor reader shipped with the existing compiler plugin. Its
// runtime FieldInfo contains custom options, but omits standard deprecated.
const pluginRequire = createRequire(resolve(protobufTsPluginRoot, 'package.json'))
const { fromBinary } = pluginRequire('@bufbuild/protobuf')
const { FileDescriptorSetSchema } = pluginRequire('@bufbuild/protobuf/wkt')

const protoFiles = [
  'annotations.proto',
  'common.proto',
  'acl.proto',
  'api_instance.proto',
  'api_manage.proto',
  'peer_rpc.proto',
  'error.proto',
]

function installGeneratedFiles(fromDir, toDir) {
  mkdirSync(toDir, { recursive: true })

  for (const entry of readdirSync(fromDir)) {
    const source = resolve(fromDir, entry)
    const target = resolve(toDir, entry)

    if (statSync(source).isDirectory()) {
      installGeneratedFiles(source, target)
      continue
    }

    renameSync(source, target)
  }
}

function findExecutableInPath(command, extensions = ['']) {
  const envPath = process.env[pathEnvKey()]
  if (typeof envPath !== 'string') return undefined

  const nodeBinSuffix = ['node_modules/.bin', 'node_modules\\.bin']
  for (const entry of envPath.split(delimiter)) {
    if (!entry || nodeBinSuffix.some((suffix) => entry.endsWith(suffix))) continue

    for (const extension of extensions) {
      const candidate = resolve(entry, `${command}${extension}`)
      if (existsSync(candidate)) return candidate
    }
  }

  return undefined
}

function pathEnvKey() {
  return Object.keys(process.env).find((key) => key.toLowerCase() === 'path') ?? 'PATH'
}

function withNodeBinPath() {
  const key = pathEnvKey()
  const currentPath = process.env[key]

  return {
    ...process.env,
    [key]: currentPath ? `${nodeBinDir}${delimiter}${currentPath}` : nodeBinDir,
  }
}

function getProtocCommand() {
  const extensions = process.platform === 'win32' ? ['.exe'] : ['']
  const systemProtoc = findExecutableInPath('protoc', extensions)

  if (systemProtoc) {
    return {
      command: systemProtoc,
      argsPrefix: ['--proto_path', relative(root, protobufTsPluginRoot)],
    }
  }

  return {
    command: process.execPath,
    argsPrefix: [protocWrapper],
  }
}

mkdirSync(generatedRoot, { recursive: true })
const tmpDir = mkdtempSync(resolve(generatedRoot, '.proto-'))
const protocCommand = getProtocCommand()
const descriptorPath = resolve(tmpDir, 'descriptors.bin')

try {
  const result = spawnSync(protocCommand.command, [
    ...protocCommand.argsPrefix,
    '-I',
    relative(root, protoRoot),
    `--ts_out=${relative(root, tmpDir)}`,
    `--descriptor_set_out=${relative(root, descriptorPath)}`,
    '--include_imports',
    '--ts_opt=use_proto_field_name,server_none,client_none,ts_nocheck',
    ...protoFiles.map((file) => relative(root, resolve(protoRoot, file))),
  ], {
    cwd: root,
    env: withNodeBinPath(),
    stdio: 'inherit',
    shell: false,
  })

  if (result.error) {
    throw result.error
  }

  const status = result.status ?? 1
  if (status === 0) {
    const descriptors = fromBinary(FileDescriptorSetSchema, readFileSync(descriptorPath))
    const networkConfig = descriptors.file.find(file => file.package === 'api.manage')
      ?.messageType.find(message => message.name === 'NetworkConfig')
    if (!networkConfig) throw new Error('The schema has no api.manage.NetworkConfig')
    const active = networkConfig.field.filter(field => !field.options?.deprecated).map(field => field.name)
    const deprecated = networkConfig.field.filter(field => field.options?.deprecated).map(field => field.name)
    const capabilityMetadata = JSON.parse(readFileSync(resolve(protoRoot, '../config-capabilities.json'), 'utf8'))
    const fieldCapabilities = {}
    for (const [field, entry] of Object.entries(capabilityMetadata)) {
      if (!active.includes(field)) throw new Error(`Configuration capability refers to unknown or deprecated field ${field}`)
      if (!entry || typeof entry !== 'object' || Object.keys(entry).length !== 2
        || typeof entry.capability !== 'string' || !entry.capability.startsWith('config:')
        || typeof entry.support !== 'string') throw new Error(`Invalid capability metadata for ${field}`)
      fieldCapabilities[field] = entry.capability
    }
    writeFileSync(resolve(tmpDir, 'configCapabilities.ts'),
      '// Generated from easytier-proto/config-capabilities.json. Do not edit.\n'
      + `export const CONFIG_FIELD_CAPABILITIES = ${JSON.stringify(fieldCapabilities, null, 2)} as const\n`)
    writeFileSync(resolve(tmpDir, 'networkConfigFields.ts'),
      '// Generated from standard Protobuf field descriptors. Do not edit.\n'
      + `export const NETWORK_CONFIG_FIELDS = ${JSON.stringify(active, null, 2)} as const\n`
      + `export const DEPRECATED_NETWORK_CONFIG_FIELDS = ${JSON.stringify(deprecated, null, 2)} as const\n`)
    rmSync(descriptorPath)
    installGeneratedFiles(tmpDir, outDir)
  }

  process.exit(status)
} finally {
  rmSync(tmpDir, { recursive: true, force: true })
}
