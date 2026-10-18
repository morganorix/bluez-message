-- bluez-message: gain on the call audio received from the iPhone over HFP (it peaks around -34 dBFS).
-- 8.0 linear = +18 dB (= `wpctl set-volume <node> 2.0`), tuned by ear on 2026-10-15.
local GAIN = 8.0

local om = ObjectManager {
  Interest {
    type = "node",
    Constraint { "node.name", "matches", "bluez_input.*", type = "pw-global" },
    Constraint { "media.role", "=", "Communication", type = "pw-global" },
  }
}

om:connect ("object-added", function (_, node)
  node:set_param ("Props", Pod.Object {
    "Spa:Pod:Object:Param:Props", "Props",
    channelVolumes = Pod.Array { "Spa:Float", GAIN },
  })
end)

om:activate ()
