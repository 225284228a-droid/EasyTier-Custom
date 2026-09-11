import { mount } from '@vue/test-utils'
import { defineComponent, h, nextTick } from 'vue'
import { describe, expect, it, vi } from 'vitest'
import UrlInput from '../src/components/UrlInput.vue'

vi.mock('vue-i18n', () => ({
  useI18n: () => ({
    t: (key: string) => key,
  }),
}))

const InputTextStub = defineComponent({
  name: 'InputText',
  props: { modelValue: String },
  emits: ['update:modelValue'],
  setup(props, { attrs, emit }) {
    return () => h('input', {
      ...attrs,
      value: props.modelValue ?? '',
      onInput: (event: Event) => emit('update:modelValue', (event.target as HTMLInputElement).value),
    })
  },
})

const InputNumberStub = defineComponent({
  name: 'InputNumber',
  props: { modelValue: Number },
  emits: ['update:modelValue'],
  setup(props, { attrs, emit }) {
    return () => h('input', {
      ...attrs,
      type: 'number',
      value: props.modelValue ?? '',
      onInput: (event: Event) => emit('update:modelValue', Number((event.target as HTMLInputElement).value)),
    })
  },
})

const AutoCompleteStub = defineComponent({
  name: 'AutoComplete',
  props: { modelValue: String },
  emits: ['update:modelValue'],
  setup(props, { attrs, emit }) {
    return () => h('input', {
      ...attrs,
      value: props.modelValue ?? '',
      onInput: (event: Event) => emit('update:modelValue', (event.target as HTMLInputElement).value),
    })
  },
})

const DialogStub = defineComponent({
  name: 'Dialog',
  props: { visible: Boolean },
  setup(props, { slots }) {
    return () => props.visible ? h('div', slots.default?.()) : null
  },
})

const ButtonStub = defineComponent({
  name: 'Button',
  setup(_, { slots }) {
    return () => h('button', slots.default?.())
  },
})

const PassThrough = defineComponent({
  name: 'PassThrough',
  setup(_, { slots }) {
    return () => h('div', slots.default?.())
  },
})

function mountUrl(url: string) {
  const model = { value: url }
  const wrapper = mount(UrlInput, {
    props: {
      protos: { tcp: 11010, wss: 443, http3: 11014 },
      modelValue: model.value,
      'onUpdate:modelValue': (value: string) => { model.value = value },
    },
    global: {
      stubs: {
        AutoComplete: AutoCompleteStub,
        Button: ButtonStub,
        Dialog: DialogStub,
        InputGroup: PassThrough,
        InputGroupAddon: PassThrough,
        InputNumber: InputNumberStub,
        InputText: InputTextStub,
      },
    },
  })
  return { model, wrapper }
}

describe('UrlInput transport options', () => {
  it('does not expose per-URL SNI controls', async () => {
    const { wrapper } = mountUrl('http3://192.0.2.1:11014')
    await nextTick()

    expect(wrapper.find('[data-sni-input]').exists()).toBe(false)
  })

  it('keeps path, query and fragment when an URL is edited', async () => {
    const { model, wrapper } = mountUrl('wss://peer.example:443/path?token=abc#fragment')
    await nextTick()

    await wrapper.find<HTMLInputElement>('[data-url-host]').setValue('other.example')
    await nextTick()

    expect(model.value).toBe('wss://other.example:443/path?token=abc#fragment')
  })
})
