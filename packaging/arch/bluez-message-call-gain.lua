-- bluez-message : gain du son d'appel reçu de l'iPhone en HFP (il arrive vers -34 dBFS en crête).
-- 8.0 linéaire = +18 dB (= `wpctl set-volume <nœud> 2.0`), validé à l'oreille le 2026-10-15.
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
