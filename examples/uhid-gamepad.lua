-- KeyForge UHID sample: declare a virtual gamepad + keyboard from Lua.
-- Drop this file into the plugin directory; the plugin UI lists it like any
-- other script. The daemon creates each named device once and reuses it
-- across config reloads.
--
-- Flow per physical event:
--   1. process() mirrors the left stick onto the virtual gamepad.
--   2. uh.poll() drains kernel events (LED output, open/close, reports).

local pad = uh.create({
    name = "KeyForge Pad",
    phys = "keyforge/input0",
    bus = uh.BUS_USB,
    vendor = 0x045e,
    product = 0x02e0,
    version = 1,
    descriptor = uh.gamepad({ buttons = 16 }).descriptor,
    kind = "gamepad",
    buttons = 16,
})

local kbd = uh.create({
    name = "KeyForge Keys",
    bus = uh.BUS_USB,
    vendor = 0x045e,
    product = 0x02e1,
    version = 1,
    descriptor = uh.keyboard().descriptor,
    kind = "keyboard",
})

return {
    id = "uhid_gamepad",
    name = "UHID Gamepad",
    version = "1.0.0",
    author = "keyforge",
    description = "Mirrors the left stick onto a UHID virtual gamepad.",
    settings = {},
    process = function(ev, cfg, pf)
        if ev.kind == "stick" and ev.side == "left" then
            pad:input({ x = ev.x, y = ev.y, buttons = {} })
        end
        for _, item in ipairs(uh.poll()) do
            local event = item.event
            if event.type == "output" then
                pf.log("uhid output rtype=" .. event.rtype)
            elseif event.type == "get_report" then
                pf.log("uhid get_report id=" .. event.id)
            end
        end
        return ev
    end,
}
