-- jitflags: --fflags=DebugLuauUserDefinedClasses=true
-- User-defined classes: NewClass/NewClassMember/Construct/FinConstruct,
-- private fields, inheritance, method calls and class metadata.
class Base
    public x
    private _secret

    function get(self)
        return self.x
    end

    function set_secret(self, value)
        self._secret = value
    end

    function secret(self)
        return self._secret
    end
end

class Child extends Base
    public y

    function sum(self)
        return self.x + self.y
    end
end

local c = Child { x = 4, y = 5 }
print(c:sum(), c:get(), c:secret())
c:set_secret(42)
print(c:secret(), class.classof(c) == Child, class.isinstance(c, Base))

local function make(C, args)
    return C(args)
end

local d = make(Child, { x = 1, y = 2 })
print(d:sum(), d.x, d.y, d:secret())

local c2 = Child { x = 4, y = 5 }
print(c == c2, class.classof(c2) == Child)
